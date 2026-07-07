use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{Display, Write},
    fs::File,
    io::{Read, Write as IoWrite},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use graft::core::{
    LogId, PageIdx, VolumeId,
    logref::LogRef,
    lsn::{LSN, LSNRangeExt},
    page::{PAGESIZE, Page},
};
use graft::remote::{Remote, RemoteConfig};
use graft::repo::{
    BranchInfo, BranchUpstream, CheckoutPlan, CommitArtifactState, CommitFileState, CommitObject,
    CommitTableSummary, FetchAllOutcome, FetchOutcome, Head, MergeOutcome, MergePlan, PullOutcome,
    PushAllOutcome, PushOutcome, RemoteBranchRef, RemoteInfo, RemotePruneOutcome,
    RepoArtifactAudit, RepoArtifactAuditIssueKind, RepoArtifactRepairOutcome, RepoConfigEntry,
    RepoDiff, RepoFileChange, RepoLargeFileFetchOutcome, RepoLargeFileFetchStatus,
    RepoLargeFilePruneOutcome, RepoLargeFileStatusOutcome, RepoLargeFileStatusState, RepoLogRange,
    RepoPathStorage, RepoSnapshot, RepoStatus, RepoStorageCommit, RepoTrackedPath,
    RepoTrackedPathDetail, RepoTrackedPathEntry, RepoTrackedPathKind, RepoWorktreeChangeKind,
    Repository, ResetMode, ResetOutcome, TagInfo,
};
use graft::{
    rt::runtime::Runtime, volume::AheadStatus, volume_reader::VolumeRead,
    volume_writer::VolumeWrite,
};
use indoc::{formatdoc, indoc, writedoc};
use parking_lot::Mutex;
use rusqlite::config::DbConfig;
use serde::{Deserialize, Serialize};
use sqlite_plugin::{
    vars::SQLITE_ERROR,
    vfs::{Pragma, PragmaErr},
};
use tryiter::TryIteratorExt;
use zerocopy::FromBytes;

use crate::{
    dbg::SqliteHeader,
    file::vol_file::VolFile,
    vfs::{ErrCtx, should_discover_repo},
};

const SQLITE_DATABASE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);
static ASYNC_JOBS: OnceLock<AsyncJobRegistry> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotHashPolicy {
    Strict,
    AllowHydratedMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepoSnapshotPurpose {
    Checkout,
    Diff,
    Export,
    Merge,
    Push,
    Reset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepoSnapshotRemoteMode {
    LocalOnly,
    Remote,
    LocalThenRemote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RepoSnapshotResolveSource {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RepoSnapshotResolvePolicy {
    purpose: RepoSnapshotPurpose,
    remote_mode: RepoSnapshotRemoteMode,
    hash_policy: SnapshotHashPolicy,
    normalize: bool,
}

#[derive(Debug)]
struct ResolvedRepoSnapshot {
    snapshot: RepoSnapshot,
    runtime_snapshot: graft::snapshot::Snapshot,
    source: RepoSnapshotResolveSource,
    hash_mismatches: usize,
}

struct RepoSnapshotResolver<'a> {
    runtime: &'a Runtime,
    remote: Option<Arc<Remote>>,
    policy: RepoSnapshotResolvePolicy,
}

fn async_jobs() -> &'static AsyncJobRegistry {
    ASYNC_JOBS.get_or_init(AsyncJobRegistry::default)
}

#[derive(Default)]
struct AsyncJobRegistry {
    jobs: Mutex<BTreeMap<String, AsyncJob>>,
}

impl AsyncJobRegistry {
    fn spawn_fetch(
        &self,
        repo_file: PathBuf,
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
        format: AsyncJobResultFormat,
    ) -> String {
        let id = format!("graft-job-{}", NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed));
        self.jobs
            .lock()
            .insert(id.clone(), AsyncJob::running("fetch", format));

        let job_id = id.clone();
        std::thread::spawn(move || {
            let result = Repository::discover_for_file(&repo_file)
                .map_err(|err| err.to_string())
                .and_then(|repo| {
                    match format {
                        AsyncJobResultFormat::Text => {
                            run_repo_fetch(&repo, remote, branch, refspec, all)
                        }
                        AsyncJobResultFormat::Json => {
                            run_repo_fetch_json(&repo, remote, branch, refspec, all)
                        }
                    }
                    .map_err(|err| err.to_string())
                });
            async_jobs().finish(&job_id, result);
        });

        id
    }

    fn finish(&self, id: &str, result: Result<String, String>) {
        let mut jobs = self.jobs.lock();
        if let Some(job) = jobs.get_mut(id) {
            match result {
                Ok(result) => job.finish(result),
                Err(error) => job.fail(error),
            }
        }
    }

    fn status_json(&self, id: &str) -> Result<String, ErrCtx> {
        let jobs = self.jobs.lock();
        let job = jobs.get(id).ok_or_else(|| unknown_job(id))?;
        Ok(job.status_json(id))
    }

    fn json_status(&self, id: &str) -> Result<String, ErrCtx> {
        let jobs = self.jobs.lock();
        let job = jobs.get(id).ok_or_else(|| unknown_job(id))?;
        Ok(job.json_status(id))
    }

    fn result(&self, id: &str) -> Result<String, ErrCtx> {
        let jobs = self.jobs.lock();
        let job = jobs.get(id).ok_or_else(|| unknown_job(id))?;
        match job.state {
            AsyncJobState::Running => Err(ErrCtx::PragmaErr(
                format!("job `{id}` is still running").into(),
            )),
            AsyncJobState::Done => Ok(job.result.clone().unwrap_or_default()),
            AsyncJobState::Failed => Err(ErrCtx::PragmaErr(
                format!(
                    "job `{id}` failed: {}",
                    job.error.as_deref().unwrap_or("unknown error")
                )
                .into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum AsyncJobResultFormat {
    Text,
    Json,
}

impl AsyncJobResultFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
        }
    }
}

fn unknown_job(id: &str) -> ErrCtx {
    ErrCtx::PragmaErr(format!("unknown job `{id}`").into())
}

#[derive(Debug, Clone)]
struct AsyncJob {
    kind: &'static str,
    format: AsyncJobResultFormat,
    state: AsyncJobState,
    result: Option<String>,
    error: Option<String>,
}

impl AsyncJob {
    fn running(kind: &'static str, format: AsyncJobResultFormat) -> Self {
        Self {
            kind,
            format,
            state: AsyncJobState::Running,
            result: None,
            error: None,
        }
    }

    fn finish(&mut self, result: String) {
        self.state = AsyncJobState::Done;
        self.result = Some(result);
        self.error = None;
    }

    fn fail(&mut self, error: String) {
        self.state = AsyncJobState::Failed;
        self.result = None;
        self.error = Some(error);
    }

    fn status_json(&self, id: &str) -> String {
        serde_json::json!({
            "id": id,
            "kind": self.kind,
            "state": self.state.as_str(),
            "result": self.result,
            "error": self.error,
        })
        .to_string()
    }

    fn json_status(&self, id: &str) -> String {
        let result = match (&self.result, self.format) {
            (Some(result), AsyncJobResultFormat::Json) => serde_json::from_str(result)
                .unwrap_or_else(|_| serde_json::Value::String(result.clone())),
            (Some(result), AsyncJobResultFormat::Text) => serde_json::Value::String(result.clone()),
            (None, _) => serde_json::Value::Null,
        };
        serde_json::json!({
            "id": id,
            "kind": self.kind,
            "state": self.state.as_str(),
            "result_format": self.format.as_str(),
            "result": result,
            "error": self.error,
        })
        .to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AsyncJobState {
    Running,
    Done,
    Failed,
}

impl AsyncJobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<String, ErrCtx> {
    serde_json::to_string(value).map_err(|e| ErrCtx::PragmaErr(format!("JSON error: {e}").into()))
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize)]
struct JsonBranchList {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    branches: Vec<BranchInfo>,
    remote_branches: Vec<RemoteBranchRef>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonFetchCommandOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    remote: String,
    branches: Vec<FetchOutcome>,
    commits: usize,
}

#[derive(Debug, Clone)]
enum FetchCommandOutcome {
    One(FetchOutcome),
    Many(FetchAllOutcome),
}

impl FetchCommandOutcome {
    fn remote(&self) -> String {
        match self {
            Self::One(outcome) => outcome.remote.clone(),
            Self::Many(outcome) => outcome.remote.clone(),
        }
    }

    fn branches(&self) -> Vec<FetchOutcome> {
        match self {
            Self::One(outcome) => vec![outcome.clone()],
            Self::Many(outcome) => outcome.branches.clone(),
        }
    }

    fn commits(&self) -> usize {
        self.branches().iter().map(|branch| branch.commits).sum()
    }
}

#[derive(Debug, Clone, Serialize)]
struct JsonPushCommandOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    remote: String,
    branches: Vec<PushOutcome>,
    commits: usize,
    forced: bool,
}

#[derive(Debug, Clone)]
enum PushCommandOutcome {
    One(PushOutcome),
    Many(PushAllOutcome),
}

impl PushCommandOutcome {
    fn remote(&self) -> String {
        match self {
            Self::One(outcome) => outcome.remote.clone(),
            Self::Many(outcome) => outcome.remote.clone(),
        }
    }

    fn branches(&self) -> Vec<PushOutcome> {
        match self {
            Self::One(outcome) => vec![outcome.clone()],
            Self::Many(outcome) => outcome.branches.clone(),
        }
    }

    fn commits(&self) -> usize {
        self.branches().iter().map(|branch| branch.commits).sum()
    }

    fn forced(&self) -> bool {
        self.branches().iter().any(|branch| branch.forced)
    }
}

#[derive(Debug, Clone, Serialize)]
struct JsonPullCommandOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    current_branch: Option<String>,
    #[serde(flatten)]
    outcome: PullOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<JsonPathAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_analysis: Option<JsonRowMergeAnalysis>,
}

#[derive(Debug, Clone)]
struct RepoPullCommandOutcome {
    outcome: PullOutcome,
    current_head: Option<String>,
    current_branch: Option<String>,
    paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonMergeCommandOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    branch: Option<String>,
    #[serde(flatten)]
    outcome: MergeOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<JsonPathAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_analysis: Option<JsonRowMergeAnalysis>,
}

#[derive(Debug)]
struct RepoMergeCommandOutcome {
    outcome: MergeOutcome,
    branch: Option<String>,
    paths: Vec<JsonPathAction>,
    row_auto_merge: Option<RowAutoMergeResult>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonMergeAbortCommandOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    head: String,
    branch: Option<String>,
    target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<JsonPathAction>,
}

#[derive(Debug)]
struct RepoMergeAbortCommandOutcome {
    target: String,
    branch: Option<String>,
    paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonMergeContinueCommandOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    head: String,
    branch: Option<String>,
    commit: JsonCommitSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<crate::json::JsonRepoPathDiff>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonCommitOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    head: String,
    branch: Option<String>,
    commit: JsonCommitSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<crate::json::JsonRepoPathDiff>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonCommitSummary {
    id: String,
    message: String,
    parents: Vec<String>,
}

#[derive(Debug, Clone)]
struct RepoCommitOutcome {
    commit: CommitObject,
    branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRepoStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(flatten)]
    status: RepoStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_analysis: Option<JsonRowMergeAnalysis>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatusSpec {
    pub(crate) kind: Option<RepoTrackedPathKind>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonLsFilesOutcome<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    stage: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    details: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    others: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    paths: Vec<T>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LsFilesSpec {
    pub(crate) stage: bool,
    pub(crate) details: bool,
    pub(crate) others: bool,
    pub(crate) kind: Option<RepoTrackedPathKind>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonInitOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    graft_dir: String,
    worktree: String,
    path: String,
    kind: &'static str,
    preserved_contents: bool,
}

#[derive(Debug, Clone)]
struct RepoInitOutcome {
    graft_dir: PathBuf,
    worktree: PathBuf,
    path: String,
    preserved_contents: bool,
    current_head: Option<String>,
    current_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonAddOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<crate::json::JsonRepoPathDiff>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRemoveOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    cached: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonSwitchOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonBranchMutationOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    branch: BranchInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonTagMutationOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    tag: TagInfo,
}

#[derive(Debug, Clone, Serialize)]
struct JsonTagListOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    tags: Vec<TagInfo>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRemoteInfo {
    name: String,
    config: RemoteConfig,
    url: String,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRemoteList {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    remotes: Vec<JsonRemoteInfo>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRemoteMutationOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    remote: JsonRemoteInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonCloneOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    remote: JsonRemoteInfo,
    branch: String,
    head: String,
    commits: usize,
    graft_dir: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<JsonPathAction>,
}

#[derive(Debug)]
struct RepoCloneOutcome {
    remote: RemoteInfo,
    current_head: Option<String>,
    current_branch: Option<String>,
    branch: String,
    head: String,
    commits: usize,
    graft_dir: PathBuf,
    paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRemotePruneCommandOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(flatten)]
    outcome: RemotePruneOutcome,
}

#[derive(Debug, Clone, Serialize)]
struct JsonConfigEntryOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(flatten)]
    entry: RepoConfigEntry,
}

#[derive(Debug, Clone, Serialize)]
struct JsonConfigListOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    entries: Vec<RepoConfigEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonConfigMutationOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    entry: RepoConfigEntry,
}

#[derive(Debug, Clone, Serialize)]
struct JsonLsRemoteOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    remote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_branch: Option<String>,
    refs: Vec<RemoteBranchRef>,
}

#[derive(Debug)]
struct RepoSwitchOutcome {
    branch: String,
    target: Option<String>,
    paths: Vec<JsonPathAction>,
}

#[derive(Debug)]
struct RepoSwitchCreateOutcome {
    branch: BranchInfo,
    paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonCheckoutOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    head: Option<String>,
    branch: Option<String>,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    path_details: Vec<JsonPathDetail>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonPathDetail {
    path: String,
    kind: &'static str,
    storage: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRestoreOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    staged: bool,
    all: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    path_details: Vec<JsonPathDetail>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonExportOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    path: String,
    kind: &'static str,
    output: String,
}

#[derive(Debug, Clone, Serialize)]
struct JsonResetCommandOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    head: String,
    branch: Option<String>,
    #[serde(flatten)]
    outcome: ResetOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonPathAction {
    path: String,
    kind: &'static str,
    storage: &'static str,
    action: &'static str,
}

#[derive(Debug, Clone)]
struct RepoResetCommandOutcome {
    outcome: ResetOutcome,
    branch: Option<String>,
    paths: Vec<JsonPathAction>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRowMergeAnalysis {
    path: String,
    available: bool,
    can_auto_merge: bool,
    ours_changes: usize,
    theirs_changes: usize,
    apply_changes: usize,
    opaque_changes: usize,
    resolved_opaque_changes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    resolved_opaque_change_details: Vec<JsonResolvedOpaqueChange>,
    apply_policy: JsonRowMergeApplyPolicy,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    limitations: Vec<crate::json::JsonDiffLimitation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blocked_reasons: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    row_conflicts: Vec<JsonRowMergeConflict>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    schema_conflicts: Vec<JsonSchemaMergeConflict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonResolvedOpaqueChange {
    name: String,
    reason: &'static str,
    resolver: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRowMergeApplyPolicy {
    foreign_keys: &'static str,
    triggers: &'static str,
    validation: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    default_semantic_keys: Vec<String>,
    internal_resolvers: Vec<JsonRowMergeInternalResolver>,
    schema_resolvers: Vec<JsonRowMergeSchemaResolver>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    generated_columns: Vec<JsonRowMergeGeneratedColumns>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRowMergeInternalResolver {
    table: String,
    resolver: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRowMergeSchemaResolver {
    operation: String,
    resolver: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRowMergeGeneratedColumns {
    table: String,
    columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRowMergeConflict {
    reason: &'static str,
    table: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    columns: Vec<String>,
    rowid: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ours_rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theirs_rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_key: Option<Vec<String>>,
    ours: &'static str,
    theirs: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ours_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theirs_row: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonSchemaMergeConflict {
    reason: &'static str,
    name: String,
    entry_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ours: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theirs: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    column_changes: Vec<JsonSchemaColumnChange>,
    message: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct JsonSchemaColumnChange {
    side: &'static str,
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonConflictList {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_head: Option<String>,
    paths: Vec<JsonConflictPath>,
    conflicts: Vec<JsonConflictArtifact>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonConflictPath {
    path: String,
    kind: &'static str,
    storage: &'static str,
    status: &'static str,
    total: usize,
    unresolved: usize,
    resolved: usize,
}

#[derive(Debug, Clone, Serialize)]
struct JsonConflictArtifact {
    id: String,
    path: String,
    path_kind: &'static str,
    storage: &'static str,
    kind: &'static str,
    reason: &'static str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    columns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ours_rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theirs_rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    semantic_key: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entry_type: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    column_changes: Vec<JsonSchemaColumnChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    change: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ours_op: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theirs_op: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ours_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theirs_row: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonResolveConflictOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    path: String,
    path_kind: &'static str,
    storage: &'static str,
    resolution: &'static str,
    remaining_conflicts: usize,
}

#[derive(Debug, Clone)]
struct RepoResolveConflictOutcome {
    path: String,
    path_kind: RepoTrackedPathKind,
    path_storage: RepoPathStorage,
}

#[derive(Debug, Clone, Serialize)]
struct JsonVolumeListEntry {
    id: String,
    local: String,
    remote: String,
    status: String,
    current: bool,
}

#[derive(Debug, Clone, Serialize)]
struct JsonVolumeAudit {
    local_pages: usize,
    total_pages: usize,
    percentage: f64,
    needs_hydrate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRepoArtifactAudit {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(flatten)]
    audit: RepoArtifactAudit,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRepoArtifactRepair {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(flatten)]
    outcome: RepoArtifactRepairOutcome,
}

#[derive(Debug, Clone, Serialize)]
struct JsonLargeFilePruneOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(flatten)]
    outcome: RepoLargeFilePruneOutcome,
}

#[derive(Debug, Clone, Serialize)]
struct JsonLargeFileFetchOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(flatten)]
    outcome: RepoLargeFileFetchOutcome,
}

#[derive(Debug, Clone, Serialize)]
struct JsonLargeFileStatusOutcome {
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(flatten)]
    outcome: RepoLargeFileStatusOutcome,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRepoDiffOutcome<T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(flatten)]
    diff: T,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRepoShowOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    #[serde(flatten)]
    commit: CommitObject,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRepoLogOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    current_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_branch: Option<String>,
    commits: Vec<CommitObject>,
}

/// Helper to create pragma errors concisely
fn pragma_fail(msg: impl Display) -> PragmaErr {
    PragmaErr::Fail(SQLITE_ERROR, Some(msg.to_string()))
}

/// Helper to parse with automatic error conversion
fn parse_or_fail<T>(s: &str) -> Result<T, PragmaErr>
where
    T: FromStr,
    T::Err: Display,
{
    s.parse().map_err(pragma_fail)
}

/// Helper to parse an optional value from colon-separated parts
fn parse_optional<T: FromStr>(s: Option<&&str>) -> Result<Option<T>, PragmaErr>
where
    T::Err: Display,
{
    s.map(|s| parse_or_fail(s)).transpose()
}

/// Extension trait for Pragma to get required arguments
trait PragmaExt<'a> {
    fn require_arg(&self) -> Result<&'a str, PragmaErr>;
}

impl<'a> PragmaExt<'a> for Pragma<'a> {
    fn require_arg(&self) -> Result<&'a str, PragmaErr> {
        self.arg.ok_or_else(|| PragmaErr::required_arg(self))
    }
}

/// Diff granularity mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    /// Default: page-level + table-level
    Default,
    /// Row-level: detailed comparison of each row
    Rows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonLogMode {
    LegacyArray,
    WithStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonConfigListMode {
    LegacyArray,
    WithStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonTagsMode {
    LegacyArray,
    WithStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonFetchAsyncMode {
    LegacyId,
    WithStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoDiffSpec {
    mode: DiffMode,
    kind: Option<RepoTrackedPathKind>,
    target: RepoDiffTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepoDiffTarget {
    Worktree {
        path: Option<String>,
    },
    Staged {
        path: Option<String>,
    },
    RevisionToWorktree {
        rev: String,
        path: Option<String>,
    },
    Revisions {
        from: String,
        to: String,
        path: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoAddSpec {
    path: Option<PathBuf>,
    force: bool,
    all: bool,
    kind: Option<RepoTrackedPathKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoRemoveSpec {
    path: Option<PathBuf>,
    cached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoAuditSpec {
    repair: bool,
    remote: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LargeFilePruneSpec {
    dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LargeFileFetchSpec {
    remote: Option<String>,
    rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LargeFileStatusSpec {
    rev: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepoCheckoutSpec {
    Detach { rev: String, force: bool },
    Path { rev: String, path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoRestoreSpec {
    source: Option<String>,
    staged: bool,
    all: bool,
    kind: Option<RepoTrackedPathKind>,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoExportSpec {
    source: Option<String>,
    path: Option<PathBuf>,
    output: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoCloneSpec {
    config: RemoteConfig,
    branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveSide {
    Ours,
    Theirs,
    Manual,
}

impl ResolveSide {
    fn index_stage(self) -> Option<graft::repo::index::IndexStage> {
        match self {
            Self::Ours => Some(graft::repo::index::IndexStage::Ours),
            Self::Theirs => Some(graft::repo::index::IndexStage::Theirs),
            Self::Manual => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ours => "ours",
            Self::Theirs => "theirs",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoResolveSpec {
    side: ResolveSide,
    path: Option<PathBuf>,
    row: Option<RepoResolveRowSpec>,
}

enum RepoConflictSideState {
    SqliteDatabase(CommitFileState),
    Artifact(CommitArtifactState),
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoResolveRowSpec {
    table: String,
    rowid: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RowConflictResolutionState {
    merge_head: Option<String>,
    rows: BTreeMap<String, String>,
}

pub(crate) enum GraftPragma {
    /// `pragma graft_debug_volume_list;`
    VolumeList,

    /// `pragma graft_debug_volume_json_list;`
    VolumeJsonList,

    /// `pragma graft_tags;`
    Tags,

    /// `pragma graft_json_tags [= "--with-status"];`
    JsonTags { mode: JsonTagsMode },

    /// `pragma graft_debug_volume_tags;`
    VolumeTags,

    /// `pragma graft_debug_volume_switch = "local_vid[:local[:remote]]";`
    VolumeSwitch {
        vid: VolumeId,
        local: Option<LogId>,
        remote: Option<LogId>,
    },

    /// `pragma graft_debug_volume_clone [= "remote"];`
    VolumeClone { remote: Option<LogId> },

    /// `pragma graft_debug_volume_fork;`
    VolumeFork,

    /// `pragma graft_checkout = "[--force] rev [-- path]";`
    RepoCheckout { spec: RepoCheckoutSpec },

    /// `pragma graft_json_checkout = "[--force] rev [-- path]";`
    JsonRepoCheckout { spec: RepoCheckoutSpec },

    /// `pragma graft_restore = "[--source rev] path|--staged --all [--kind kind]";`
    Restore { spec: RepoRestoreSpec },

    /// `pragma graft_json_restore = "[--source rev] path|--staged --all [--kind kind]";`
    JsonRestore { spec: RepoRestoreSpec },

    /// `pragma graft_export = "[--source rev] --output output.db [-- path]";`
    Export { spec: RepoExportSpec },

    /// `pragma graft_json_export = "[--source rev] --output output.db [-- path]";`
    JsonExport { spec: RepoExportSpec },

    /// `pragma graft_debug_volume_info;`
    VolumeInfo,

    /// `pragma graft_status [= "[--kind kind]"];`
    Status { spec: StatusSpec },

    /// `pragma graft_debug_volume_status;`
    VolumeStatus,

    /// `pragma graft_init;`
    RepoInit,

    /// `pragma graft_json_init;`
    JsonRepoInit,

    /// `pragma graft_clone = "remote-uri [branch]";`
    RepoClone { spec: RepoCloneSpec },

    /// `pragma graft_json_clone = "remote-uri [branch]";`
    JsonRepoClone { spec: RepoCloneSpec },

    /// `pragma graft_json_status [= "[--kind kind]"];`
    JsonStatus { spec: StatusSpec },

    /// `pragma graft_add = "[--all|-A] [--kind kind]|[--force] [path]";`
    Add { spec: RepoAddSpec },

    /// `pragma graft_json_add = "[--all|-A] [--kind kind]|[--force] [path]";`
    JsonAdd { spec: RepoAddSpec },

    /// `pragma graft_rm = "[--cached] [path]";`
    Remove { spec: RepoRemoveSpec },

    /// `pragma graft_json_rm = "[--cached] [path]";`
    JsonRemove { spec: RepoRemoveSpec },

    /// `pragma graft_commit = "message";`
    Commit { message: String },

    /// `pragma graft_json_commit = "message";`
    JsonCommit { message: String },

    /// `pragma graft_branch [= "-r|--remote|-a|--all"];`
    Branch { mode: BranchListMode },

    /// `pragma graft_json_branch [= "-r|--remote|-a|--all"];`
    JsonBranch { mode: BranchListMode },

    /// `pragma graft_branch_create = "name [start-point]";`
    BranchCreate {
        name: String,
        start_point: Option<String>,
    },

    /// `pragma graft_json_branch_create = "name [start-point]";`
    JsonBranchCreate {
        name: String,
        start_point: Option<String>,
    },

    /// `pragma graft_branch_delete = "[--force] name";`
    BranchDelete { name: String, force: bool },

    /// `pragma graft_json_branch_delete = "[--force] name";`
    JsonBranchDelete { name: String, force: bool },

    /// `pragma graft_branch_rename = "[--force] [old] new";`
    BranchRename {
        old: Option<String>,
        new: String,
        force: bool,
    },

    /// `pragma graft_json_branch_rename = "[--force] [old] new";`
    JsonBranchRename {
        old: Option<String>,
        new: String,
        force: bool,
    },

    /// `pragma graft_branch_upstream = "[branch] remote/branch";`
    BranchUpstream {
        branch: Option<String>,
        remote: String,
        remote_branch: String,
    },

    /// `pragma graft_json_branch_upstream = "[branch] remote/branch";`
    JsonBranchUpstream {
        branch: Option<String>,
        remote: String,
        remote_branch: String,
    },

    /// `pragma graft_branch_unset_upstream [= "branch"];`
    BranchUnsetUpstream { branch: Option<String> },

    /// `pragma graft_json_branch_unset_upstream [= "branch"];`
    JsonBranchUnsetUpstream { branch: Option<String> },

    /// `pragma graft_tag_create = "name [rev]";`
    /// `pragma graft_tag_create = "--annotated name [rev] -- message";`
    TagCreate {
        name: String,
        target: Option<String>,
        message: Option<String>,
    },

    /// `pragma graft_json_tag_create = "name [rev]";`
    /// `pragma graft_json_tag_create = "--annotated name [rev] -- message";`
    JsonTagCreate {
        name: String,
        target: Option<String>,
        message: Option<String>,
    },

    /// `pragma graft_tag_delete = "name";`
    TagDelete { name: String },

    /// `pragma graft_json_tag_delete = "name";`
    JsonTagDelete { name: String },

    /// `pragma graft_switch_branch = "[--force] name";`
    SwitchBranch { name: String, force: bool },

    /// `pragma graft_json_switch_branch = "[--force] name";`
    JsonSwitchBranch { name: String, force: bool },

    /// `pragma graft_switch_create = "[--force] name [start-point]";`
    SwitchCreate {
        name: String,
        start_point: Option<String>,
        force: bool,
    },

    /// `pragma graft_json_switch_create = "[--force] name [start-point]";`
    JsonSwitchCreate {
        name: String,
        start_point: Option<String>,
        force: bool,
    },

    /// `pragma graft_merge = "rev";`
    Merge { rev: String },

    /// `pragma graft_json_merge = "rev";`
    JsonMerge { rev: String },

    /// `pragma graft_merge_abort;`
    MergeAbort,

    /// `pragma graft_json_merge_abort;`
    JsonMergeAbort,

    /// `pragma graft_merge_continue = "message";`
    MergeContinue { message: String },

    /// `pragma graft_json_merge_continue = "message";`
    JsonMergeContinue { message: String },

    /// `pragma graft_conflicts;`
    Conflicts,

    /// `pragma graft_json_conflicts;`
    JsonConflicts,

    /// `pragma graft_resolve = "--ours|--theirs|--manual [path]";`
    Resolve { spec: RepoResolveSpec },

    /// `pragma graft_json_resolve_conflict = "--ours|--theirs|--manual [path]";`
    JsonResolveConflict { spec: RepoResolveSpec },

    /// `pragma graft_remote_add = "name remote-uri";`
    RemoteAdd { name: String, config: RemoteConfig },

    /// `pragma graft_json_remote_add = "name remote-uri";`
    JsonRemoteAdd { name: String, config: RemoteConfig },

    /// `pragma graft_remote_remove = "name";`
    RemoteRemove { name: String },

    /// `pragma graft_json_remote_remove = "name";`
    JsonRemoteRemove { name: String },

    /// `pragma graft_remote_rename = "old new";`
    RemoteRename { old: String, new: String },

    /// `pragma graft_json_remote_rename = "old new";`
    JsonRemoteRename { old: String, new: String },

    /// `pragma graft_remote_get_url = "name";`
    RemoteGetUrl { name: String },

    /// `pragma graft_json_remote_get_url = "name";`
    JsonRemoteGetUrl { name: String },

    /// `pragma graft_remote_set_url = "name remote-uri";`
    RemoteSetUrl { name: String, config: RemoteConfig },

    /// `pragma graft_json_remote_set_url = "name remote-uri";`
    JsonRemoteSetUrl { name: String, config: RemoteConfig },

    /// `pragma graft_remote_prune = "name";`
    RemotePrune { name: String },

    /// `pragma graft_json_remote_prune = "name";`
    JsonRemotePrune { name: String },

    /// `pragma graft_ls_remote = "name";`
    LsRemote { name: String },

    /// `pragma graft_json_ls_remote = "name";`
    JsonLsRemote { name: String },

    /// `pragma graft_remotes;`
    Remotes,

    /// `pragma graft_json_remotes;`
    JsonRemotes,

    /// `pragma graft_debug_volume_snapshot;`
    VolumeSnapshot,

    /// `pragma graft_fetch;`
    Fetch {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_json_fetch;`
    JsonFetch {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_fetch_async;`
    FetchAsync {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_json_fetch_async;`
    JsonFetchAsync {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
        mode: JsonFetchAsyncMode,
    },

    /// `pragma graft_job_status = "job-id";`
    JobStatus { id: String },

    /// `pragma graft_json_job_status = "job-id";`
    JsonJobStatus { id: String },

    /// `pragma graft_job_result = "job-id";`
    JobResult { id: String },

    /// `pragma graft_json_job_result = "job-id";`
    JsonJobResult { id: String },

    /// `pragma graft_pull;`
    Pull {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_json_pull;`
    JsonPull {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
    },

    /// `pragma graft_push;`
    Push {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
        force: bool,
    },

    /// `pragma graft_json_push;`
    JsonPush {
        remote: Option<String>,
        branch: Option<String>,
        refspec: Option<String>,
        all: bool,
        force: bool,
    },

    /// `pragma graft_debug_volume_fetch;`
    VolumeFetch,

    /// `pragma graft_debug_volume_pull;`
    VolumePull,

    /// `pragma graft_debug_volume_push;`
    VolumePush,

    /// `pragma graft_debug_volume_audit;`
    VolumeAudit,

    /// `pragma graft_debug_volume_json_audit;`
    VolumeJsonAudit,

    /// `pragma graft_audit [= "[--repair [remote]]"];`
    RepoAudit { spec: RepoAuditSpec },

    /// `pragma graft_json_audit [= "[--repair [remote]]"];`
    JsonRepoAudit { spec: RepoAuditSpec },

    /// `pragma graft_payload_fetch [= "[--remote remote] [rev]"];`
    LargeFileFetch { spec: LargeFileFetchSpec },

    /// `pragma graft_json_payload_fetch [= "[--remote remote] [rev]"];`
    JsonLargeFileFetch {
        spec: LargeFileFetchSpec,
        operation: &'static str,
    },

    /// `pragma graft_payload_status [= "[rev]"];`
    LargeFileStatus { spec: LargeFileStatusSpec },

    /// `pragma graft_json_payload_status [= "[rev]"];`
    JsonLargeFileStatus {
        spec: LargeFileStatusSpec,
        operation: &'static str,
    },

    /// `pragma graft_payload_prune [= "[--dry-run|--force]"];`
    LargeFilePrune { spec: LargeFilePruneSpec },

    /// `pragma graft_json_payload_prune [= "[--dry-run|--force]"];`
    JsonLargeFilePrune {
        spec: LargeFilePruneSpec,
        operation: &'static str,
    },

    /// `pragma graft_ls_files [= "[--stage|--details|--others] [--kind kind]"];`
    LsFiles { spec: LsFilesSpec },

    /// `pragma graft_json_ls_files [= "[--stage|--details|--others] [--kind kind]"];`
    JsonLsFiles { spec: LsFilesSpec },

    /// `pragma graft_config_get = "key";`
    ConfigGet { key: String },

    /// `pragma graft_json_config_get = "key";`
    JsonConfigGet { key: String },

    /// `pragma graft_config_list;`
    ConfigList,

    /// `pragma graft_json_config_list [= "--with-status"];`
    JsonConfigList { mode: JsonConfigListMode },

    /// `pragma graft_config_set = "key -- value";`
    ConfigSet { key: String, value: String },

    /// `pragma graft_json_config_set = "key -- value";`
    JsonConfigSet { key: String, value: String },

    /// `pragma graft_config_unset = "key";`
    ConfigUnset { key: String },

    /// `pragma graft_json_config_unset = "key";`
    JsonConfigUnset { key: String },

    /// `pragma graft_debug_volume_hydrate;`
    VolumeHydrate,

    /// `pragma graft_version;`
    Version,

    /// `pragma graft_debug_volume_import = "PATH";`
    VolumeImport,

    /// `pragma graft_debug_volume_export = "PATH";`
    VolumeExport(PathBuf),

    /// `pragma graft_debug_volume_dump_header;`
    VolumeDumpSqliteHeader,

    /// `pragma graft_debug_volume_dump_commit = "logid:LSN";`
    VolumeDumpCommit { logref: LogRef },

    /// `pragma graft_debug_log_lsn;`
    /// Display storage commit history for the current Volume by LSN.
    DebugLogLsn,

    /// `pragma graft_debug_show_lsn = "logid:LSN";`
    /// Display storage commit details for an internal Log/LSN coordinate.
    DebugShowLsn { logref: LogRef },

    /// `pragma graft_debug_diff_lsn = "logid:from logid:to";`
    /// Compare storage commits by internal Log/LSN coordinates.
    DebugDiffLsn { from: LogRef, to: LogRef },

    /// `pragma graft_log;`
    /// Display repository commit history
    Log,

    /// `pragma graft_debug_volume_checkout_lsn = "LSN";`
    /// Checkout to specified local LSN (creates new Volume)
    VolumeCheckoutLsn { lsn: LSN },

    /// `pragma graft_debug_volume_reset_to = "LSN";`
    /// Reset current tag to specified LSN
    VolumeResetTo { lsn: LSN },

    /// `pragma graft_reset = "[--soft|--mixed|--hard] rev";`
    /// Reset the current repository branch to a revision
    Reset { rev: String, mode: ResetMode },

    /// `pragma graft_json_reset = "[--soft|--mixed|--hard] rev";`
    /// Reset the current repository branch to a revision and return JSON
    JsonReset { rev: String, mode: ResetMode },

    /// `pragma graft_debug_volume_diff = "from_lsn,to_lsn[,mode]";`
    /// Compare legacy Volume commits by LSN
    /// mode: omitted=default (page + table level), "rows"=row-level detailed comparison
    VolumeDiff { from: LSN, to: LSN, mode: DiffMode },

    /// `pragma graft_diff = "[--rows] [--kind kind] [--staged] [rev] [rev] [-- path]";`
    /// Compare repository commits by revision syntax
    RepoDiff { spec: RepoDiffSpec },

    /// `pragma graft_show = "rev";`
    /// Display detailed info for specified revision
    Show { target: String },

    // JSON output variants (non-breaking additions)
    /// `pragma graft_json_log [= "--with-status"];`
    /// Repository commit history as JSON array, or app-facing JSON object with status
    JsonLog { mode: JsonLogMode },

    /// `pragma graft_debug_volume_json_diff = "from_lsn,to_lsn[,mode]";`
    /// Legacy Volume diff as JSON. mode: omitted=summary, "rows"=row-level detail
    VolumeJsonDiff { from: LSN, to: LSN, mode: DiffMode },

    /// `pragma graft_json_diff = "[--rows] [--kind kind] [--staged] [rev] [rev] [-- path]";`
    /// Repository diff as JSON
    JsonRepoDiff { spec: RepoDiffSpec },

    /// `pragma graft_json_show = "rev";`
    /// Commit details as JSON
    JsonShow { target: String },

    /// `pragma graft_debug_volume_json_info;`
    /// Volume info as JSON
    VolumeJsonInfo,

    /// `pragma graft_debug_volume_table_log = 'table_name';`
    /// Show commits that modified the given table
    VolumeTableLog { table: String },

    /// `pragma graft_debug_volume_json_table_log = 'table_name';`
    /// Show commits that modified the given table, as JSON
    VolumeJsonTableLog { table: String },

    /// `pragma graft_debug_volume_set_message = 'message';`
    /// Set a human-readable message for the next commit
    VolumeSetMessage { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchListMode {
    Local,
    Remote,
    All,
}

impl BranchListMode {
    fn includes_remote(self) -> bool {
        matches!(self, Self::Remote | Self::All)
    }
}

impl TryFrom<&Pragma<'_>> for GraftPragma {
    type Error = PragmaErr;

    fn try_from(p: &Pragma<'_>) -> Result<Self, Self::Error> {
        if let Some((prefix, suffix)) = p.name.split_once("_")
            && prefix == "graft"
        {
            return match suffix {
                "debug_volume_list" => Ok(GraftPragma::VolumeList),
                "debug_volume_json_list" => Ok(GraftPragma::VolumeJsonList),
                "tags" => Ok(GraftPragma::Tags),
                "json_tags" => Ok(GraftPragma::JsonTags { mode: parse_json_tags_arg(p.arg)? }),
                "debug_volume_tags" => Ok(GraftPragma::VolumeTags),
                "debug_volume_clone" => {
                    let remote = p.arg.map(parse_or_fail).transpose()?;
                    Ok(GraftPragma::VolumeClone { remote })
                }
                "debug_volume_fork" => Ok(GraftPragma::VolumeFork),
                "checkout" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_checkout_arg(arg)?;
                    Ok(GraftPragma::RepoCheckout { spec })
                }
                "json_checkout" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_checkout_arg(arg)?;
                    Ok(GraftPragma::JsonRepoCheckout { spec })
                }
                "restore" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_restore_arg(arg)?;
                    Ok(GraftPragma::Restore { spec })
                }
                "json_restore" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_restore_arg(arg)?;
                    Ok(GraftPragma::JsonRestore { spec })
                }
                "export" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_export_arg(arg)?;
                    Ok(GraftPragma::Export { spec })
                }
                "json_export" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_export_arg(arg)?;
                    Ok(GraftPragma::JsonExport { spec })
                }
                "debug_volume_new" => Ok(GraftPragma::VolumeSwitch {
                    vid: VolumeId::random(),
                    local: None,
                    remote: None,
                }),
                "debug_volume_switch" => {
                    let parts: Vec<&str> = p.require_arg()?.split(':').collect();
                    if parts.is_empty() || parts.len() > 3 {
                        return Err(pragma_fail(
                            "argument must be in the form: `local_vid[:local[:remote]]`",
                        ));
                    }
                    Ok(GraftPragma::VolumeSwitch {
                        vid: parse_or_fail(parts[0])?,
                        local: parse_optional(parts.get(1))?,
                        remote: parse_optional(parts.get(2))?,
                    })
                }
                "debug_volume_info" => Ok(GraftPragma::VolumeInfo),
                "status" => Ok(GraftPragma::Status { spec: parse_status_arg(p.arg)? }),
                "debug_volume_status" => Ok(GraftPragma::VolumeStatus),
                "init" => Ok(GraftPragma::RepoInit),
                "json_init" => Ok(GraftPragma::JsonRepoInit),
                "clone" => {
                    let spec = parse_repo_clone_arg(p.require_arg()?)?;
                    Ok(GraftPragma::RepoClone { spec })
                }
                "json_clone" => {
                    let spec = parse_repo_clone_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonRepoClone { spec })
                }
                "json_status" => Ok(GraftPragma::JsonStatus { spec: parse_status_arg(p.arg)? }),
                "add" => Ok(GraftPragma::Add { spec: parse_repo_add_arg(p.arg)? }),
                "json_add" => Ok(GraftPragma::JsonAdd { spec: parse_repo_add_arg(p.arg)? }),
                "rm" => Ok(GraftPragma::Remove { spec: parse_repo_remove_arg(p.arg)? }),
                "json_rm" => Ok(GraftPragma::JsonRemove { spec: parse_repo_remove_arg(p.arg)? }),
                "commit" => Ok(GraftPragma::Commit { message: p.require_arg()?.to_string() }),
                "json_commit" => {
                    Ok(GraftPragma::JsonCommit { message: p.require_arg()?.to_string() })
                }
                "branch" => Ok(GraftPragma::Branch { mode: parse_branch_list_mode(p.arg)? }),
                "json_branch" => {
                    Ok(GraftPragma::JsonBranch { mode: parse_branch_list_mode(p.arg)? })
                }
                "branch_create" => {
                    let (name, start_point) = parse_branch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchCreate { name, start_point })
                }
                "json_branch_create" => {
                    let (name, start_point) = parse_branch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonBranchCreate { name, start_point })
                }
                "branch_delete" => {
                    let (name, force) = parse_branch_delete_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchDelete { name, force })
                }
                "json_branch_delete" => {
                    let (name, force) = parse_branch_delete_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonBranchDelete { name, force })
                }
                "branch_rename" => {
                    let (old, new, force) = parse_branch_rename_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchRename { old, new, force })
                }
                "json_branch_rename" => {
                    let (old, new, force) = parse_branch_rename_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonBranchRename { old, new, force })
                }
                "branch_upstream" => {
                    let (branch, remote, remote_branch) =
                        parse_branch_upstream_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchUpstream { branch, remote, remote_branch })
                }
                "json_branch_upstream" => {
                    let (branch, remote, remote_branch) =
                        parse_branch_upstream_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonBranchUpstream { branch, remote, remote_branch })
                }
                "branch_unset_upstream" => {
                    Ok(GraftPragma::BranchUnsetUpstream { branch: p.arg.map(str::to_string) })
                }
                "json_branch_unset_upstream" => {
                    Ok(GraftPragma::JsonBranchUnsetUpstream { branch: p.arg.map(str::to_string) })
                }
                "tag_create" => {
                    let (name, target, message) = parse_tag_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::TagCreate { name, target, message })
                }
                "json_tag_create" => {
                    let (name, target, message) = parse_tag_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonTagCreate { name, target, message })
                }
                "tag_delete" => Ok(GraftPragma::TagDelete { name: p.require_arg()?.to_string() }),
                "json_tag_delete" => {
                    Ok(GraftPragma::JsonTagDelete { name: p.require_arg()?.to_string() })
                }
                "switch_branch" => {
                    let (name, force) = parse_switch_branch_arg(p.require_arg()?)?;
                    Ok(GraftPragma::SwitchBranch { name, force })
                }
                "json_switch_branch" => {
                    let (name, force) = parse_switch_branch_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonSwitchBranch { name, force })
                }
                "switch_create" => {
                    let (name, start_point, force) = parse_switch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::SwitchCreate { name, start_point, force })
                }
                "json_switch_create" => {
                    let (name, start_point, force) = parse_switch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonSwitchCreate { name, start_point, force })
                }
                "merge" => Ok(GraftPragma::Merge { rev: p.require_arg()?.to_string() }),
                "json_merge" => Ok(GraftPragma::JsonMerge { rev: p.require_arg()?.to_string() }),
                "merge_abort" => Ok(GraftPragma::MergeAbort),
                "json_merge_abort" => Ok(GraftPragma::JsonMergeAbort),
                "merge_continue" => {
                    Ok(GraftPragma::MergeContinue { message: p.require_arg()?.to_string() })
                }
                "json_merge_continue" => {
                    Ok(GraftPragma::JsonMergeContinue { message: p.require_arg()?.to_string() })
                }
                "conflicts" => Ok(GraftPragma::Conflicts),
                "json_conflicts" => Ok(GraftPragma::JsonConflicts),
                "resolve" => Ok(GraftPragma::Resolve {
                    spec: parse_repo_resolve_arg(p.require_arg()?)?,
                }),
                "json_resolve_conflict" => Ok(GraftPragma::JsonResolveConflict {
                    spec: parse_repo_resolve_arg(p.require_arg()?)?,
                }),
                "remote_add" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::RemoteAdd { name, config })
                }
                "json_remote_add" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::JsonRemoteAdd { name, config })
                }
                "remote_remove" => {
                    Ok(GraftPragma::RemoteRemove { name: p.require_arg()?.to_string() })
                }
                "json_remote_remove" => {
                    Ok(GraftPragma::JsonRemoteRemove { name: p.require_arg()?.to_string() })
                }
                "remote_rename" => {
                    let (old, new) = parse_remote_rename(p.require_arg()?)?;
                    Ok(GraftPragma::RemoteRename { old, new })
                }
                "json_remote_rename" => {
                    let (old, new) = parse_remote_rename(p.require_arg()?)?;
                    Ok(GraftPragma::JsonRemoteRename { old, new })
                }
                "remote_get_url" => {
                    Ok(GraftPragma::RemoteGetUrl { name: p.require_arg()?.to_string() })
                }
                "json_remote_get_url" => {
                    Ok(GraftPragma::JsonRemoteGetUrl { name: p.require_arg()?.to_string() })
                }
                "remote_set_url" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::RemoteSetUrl { name, config })
                }
                "json_remote_set_url" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::JsonRemoteSetUrl { name, config })
                }
                "remote_prune" => {
                    Ok(GraftPragma::RemotePrune { name: p.require_arg()?.to_string() })
                }
                "json_remote_prune" => {
                    Ok(GraftPragma::JsonRemotePrune { name: p.require_arg()?.to_string() })
                }
                "ls_remote" => Ok(GraftPragma::LsRemote { name: p.require_arg()?.to_string() }),
                "json_ls_remote" => {
                    Ok(GraftPragma::JsonLsRemote { name: p.require_arg()?.to_string() })
                }
                "remotes" => Ok(GraftPragma::Remotes),
                "json_remotes" => Ok(GraftPragma::JsonRemotes),
                "debug_volume_snapshot" => Ok(GraftPragma::VolumeSnapshot),
                "fetch" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("fetch does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::Fetch { remote, branch, refspec, all })
                }
                "json_fetch" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_fetch does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::JsonFetch { remote, branch, refspec, all })
                }
                "fetch_async" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("fetch_async does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::FetchAsync { remote, branch, refspec, all })
                }
                "json_fetch_async" => {
                    let (arg, mode) = parse_json_fetch_async_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_fetch_async does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::JsonFetchAsync { remote, branch, refspec, all, mode })
                }
                "job_status" => Ok(GraftPragma::JobStatus { id: p.require_arg()?.to_string() }),
                "json_job_status" => {
                    Ok(GraftPragma::JsonJobStatus { id: p.require_arg()?.to_string() })
                }
                "job_result" => Ok(GraftPragma::JobResult { id: p.require_arg()?.to_string() }),
                "json_job_result" => {
                    Ok(GraftPragma::JsonJobResult { id: p.require_arg()?.to_string() })
                }
                "pull" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("pull does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::Pull { remote, branch, refspec, all })
                }
                "json_pull" => {
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_pull does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::JsonPull { remote, branch, refspec, all })
                }
                "push" => {
                    let RemoteBranchArg { remote, branch, refspec, all, force } =
                        parse_remote_branch_arg(p.arg)?;
                    Ok(GraftPragma::Push { remote, branch, refspec, all, force })
                }
                "json_push" => {
                    let RemoteBranchArg { remote, branch, refspec, all, force } =
                        parse_remote_branch_arg(p.arg)?;
                    Ok(GraftPragma::JsonPush { remote, branch, refspec, all, force })
                }
                "debug_volume_fetch" => Ok(GraftPragma::VolumeFetch),
                "debug_volume_pull" => Ok(GraftPragma::VolumePull),
                "debug_volume_push" => Ok(GraftPragma::VolumePush),
                "debug_volume_audit" => Ok(GraftPragma::VolumeAudit),
                "debug_volume_json_audit" => Ok(GraftPragma::VolumeJsonAudit),
                "audit" => Ok(GraftPragma::RepoAudit { spec: parse_repo_audit_arg(p.arg)? }),
                "json_audit" => {
                    Ok(GraftPragma::JsonRepoAudit { spec: parse_repo_audit_arg(p.arg)? })
                }
                "lfs_fetch" | "payload_fetch" => {
                    Ok(GraftPragma::LargeFileFetch { spec: parse_lfs_fetch_arg(p.arg)? })
                }
                "json_lfs_fetch" => Ok(GraftPragma::JsonLargeFileFetch {
                    spec: parse_lfs_fetch_arg(p.arg)?,
                    operation: "lfs_fetch",
                }),
                "json_payload_fetch" => Ok(GraftPragma::JsonLargeFileFetch {
                    spec: parse_lfs_fetch_arg(p.arg)?,
                    operation: "payload_fetch",
                }),
                "lfs_status" | "payload_status" => {
                    Ok(GraftPragma::LargeFileStatus { spec: parse_lfs_status_arg(p.arg)? })
                }
                "json_lfs_status" => Ok(GraftPragma::JsonLargeFileStatus {
                    spec: parse_lfs_status_arg(p.arg)?,
                    operation: "lfs_status",
                }),
                "json_payload_status" => Ok(GraftPragma::JsonLargeFileStatus {
                    spec: parse_lfs_status_arg(p.arg)?,
                    operation: "payload_status",
                }),
                "lfs_prune" | "payload_prune" => {
                    Ok(GraftPragma::LargeFilePrune { spec: parse_lfs_prune_arg(p.arg)? })
                }
                "json_lfs_prune" => Ok(GraftPragma::JsonLargeFilePrune {
                    spec: parse_lfs_prune_arg(p.arg)?,
                    operation: "lfs_prune",
                }),
                "json_payload_prune" => Ok(GraftPragma::JsonLargeFilePrune {
                    spec: parse_lfs_prune_arg(p.arg)?,
                    operation: "payload_prune",
                }),
                "ls_files" => Ok(GraftPragma::LsFiles { spec: parse_ls_files_arg(p.arg)? }),
                "json_ls_files" => {
                    Ok(GraftPragma::JsonLsFiles { spec: parse_ls_files_arg(p.arg)? })
                }
                "config_get" => Ok(GraftPragma::ConfigGet { key: p.require_arg()?.to_string() }),
                "json_config_get" => {
                    Ok(GraftPragma::JsonConfigGet { key: p.require_arg()?.to_string() })
                }
                "config_list" => Ok(GraftPragma::ConfigList),
                "json_config_list" => {
                    Ok(GraftPragma::JsonConfigList { mode: parse_json_config_list_arg(p.arg)? })
                }
                "config_set" => {
                    let (key, value) = parse_repo_config_set_arg(p.require_arg()?)?;
                    Ok(GraftPragma::ConfigSet { key, value })
                }
                "json_config_set" => {
                    let (key, value) = parse_repo_config_set_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonConfigSet { key, value })
                }
                "config_unset" => {
                    Ok(GraftPragma::ConfigUnset { key: p.require_arg()?.to_string() })
                }
                "json_config_unset" => {
                    Ok(GraftPragma::JsonConfigUnset { key: p.require_arg()?.to_string() })
                }
                "debug_volume_hydrate" => Ok(GraftPragma::VolumeHydrate),
                "version" => Ok(GraftPragma::Version),
                "debug_volume_import" => {
                    let _ = p.require_arg()?;
                    Ok(GraftPragma::VolumeImport)
                }
                "debug_volume_export" => {
                    Ok(GraftPragma::VolumeExport(PathBuf::from(p.require_arg()?)))
                }
                "debug_volume_dump_header" => Ok(GraftPragma::VolumeDumpSqliteHeader),
                "debug_volume_dump_commit" => {
                    Ok(GraftPragma::VolumeDumpCommit { logref: parse_or_fail(p.require_arg()?)? })
                }
                "debug_log_lsn" => Ok(GraftPragma::DebugLogLsn),
                "debug_show_lsn" => {
                    Ok(GraftPragma::DebugShowLsn { logref: parse_or_fail(p.require_arg()?)? })
                }
                "debug_diff_lsn" => {
                    let (from, to) = parse_debug_diff_lsn_arg(p.require_arg()?)?;
                    Ok(GraftPragma::DebugDiffLsn { from, to })
                }
                "log" => Ok(GraftPragma::Log),
                "debug_volume_checkout_lsn" => {
                    Ok(GraftPragma::VolumeCheckoutLsn { lsn: parse_or_fail(p.require_arg()?)? })
                }
                "debug_volume_reset_to" => {
                    Ok(GraftPragma::VolumeResetTo { lsn: parse_or_fail(p.require_arg()?)? })
                }
                "reset" => {
                    let (mode, rev) = parse_repo_reset_arg(p.require_arg()?)?;
                    Ok(GraftPragma::Reset { rev, mode })
                }
                "json_reset" => {
                    let (mode, rev) = parse_repo_reset_arg(p.require_arg()?)?;
                    Ok(GraftPragma::JsonReset { rev, mode })
                }
                "diff" => {
                    let spec = parse_repo_diff_arg(p.arg)?;
                    Ok(GraftPragma::RepoDiff { spec })
                }
                "debug_volume_diff" => {
                    let (from, to, mode) = parse_volume_diff_arg(p.require_arg()?)?;
                    Ok(GraftPragma::VolumeDiff { from, to, mode })
                }
                "show" => Ok(GraftPragma::Show { target: p.require_arg()?.to_string() }),
                "json_log" => Ok(GraftPragma::JsonLog { mode: parse_json_log_arg(p.arg)? }),
                "json_diff" => {
                    let spec = parse_repo_diff_arg(p.arg)?;
                    Ok(GraftPragma::JsonRepoDiff { spec })
                }
                "debug_volume_json_diff" => {
                    let (from, to, mode) = parse_volume_diff_arg(p.require_arg()?)?;
                    Ok(GraftPragma::VolumeJsonDiff { from, to, mode })
                }
                "json_show" => Ok(GraftPragma::JsonShow { target: p.require_arg()?.to_string() }),
                "debug_volume_json_info" => Ok(GraftPragma::VolumeJsonInfo),
                "debug_volume_table_log" => {
                    Ok(GraftPragma::VolumeTableLog { table: p.require_arg()?.to_string() })
                }
                "debug_volume_json_table_log" => {
                    Ok(GraftPragma::VolumeJsonTableLog { table: p.require_arg()?.to_string() })
                }
                "debug_volume_set_message" => {
                    Ok(GraftPragma::VolumeSetMessage { message: p.require_arg()?.to_string() })
                }
                _ => Err(pragma_fail(format!("invalid graft pragma `{}`", p.name))),
            };
        }
        Err(PragmaErr::NotFound)
    }
}

macro_rules! pragma_err {
    ($msg:expr) => {
        Err(ErrCtx::PragmaErr($msg.into()))
    };
}

impl GraftPragma {
    pub fn eval(self, _runtime: &Runtime, file: &mut VolFile) -> Result<Option<String>, ErrCtx> {
        let runtime = file.runtime().clone();
        match self {
            GraftPragma::VolumeList => Ok(Some(format_volumes(&runtime, file)?)),
            GraftPragma::VolumeJsonList => Ok(Some(to_json(&json_volumes(&runtime, file)?)?)),
            GraftPragma::Tags => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_tags(&repo.tags()?)?))
            }
            GraftPragma::JsonTags { mode } => {
                let repo = repo_for_file(file)?;
                let tags = repo.tags()?;
                match mode {
                    JsonTagsMode::LegacyArray => Ok(Some(to_json(&tags)?)),
                    JsonTagsMode::WithStatus => {
                        let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                        Ok(Some(to_json(&JsonTagListOutcome {
                            current_head,
                            current_branch,
                            tags,
                        })?))
                    }
                }
            }
            GraftPragma::VolumeTags => Ok(Some(format_tags(&runtime, file)?)),

            GraftPragma::VolumeClone { remote } => {
                if !file.is_idle() {
                    return pragma_err!("cannot clone while there is an open transaction");
                }

                let remote = match remote {
                    Some(remote) => remote,
                    None => runtime.volume_get(&file.vid)?.remote,
                };
                let volume = runtime.volume_open(None, None, Some(remote))?;
                file.switch_volume(&volume.vid)?;

                Ok(Some(format!(
                    "Created new Volume {} from remote Log {}",
                    volume.vid, volume.remote
                )))
            }

            GraftPragma::VolumeFork => {
                if !file.is_idle() {
                    return pragma_err!("cannot fork while there is an open transaction");
                }

                let snapshot = file.snapshot_or_latest()?;
                let missing = runtime.snapshot_missing_pages(&snapshot)?;
                if missing.is_empty() {
                    let volume = runtime.volume_from_snapshot(&snapshot)?;
                    file.switch_volume(&volume.vid)?;

                    Ok(Some(format!(
                        "Forked current snapshot into Volume: {}",
                        volume.vid,
                    )))
                } else {
                    pragma_err!("ERROR: must hydrate volume before forking")
                }
            }

            GraftPragma::RepoCheckout { spec } => {
                let outcome = run_repo_checkout(&runtime, file, spec)?;
                Ok(Some(format_checkout_outcome(&outcome)))
            }
            GraftPragma::JsonRepoCheckout { spec } => {
                let outcome = run_repo_checkout(&runtime, file, spec)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::Restore { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot restore while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let outcome = restore_repo_path(&runtime, file, &repo, &spec)?;
                Ok(Some(format_restore_outcome(&outcome)))
            }
            GraftPragma::JsonRestore { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot restore while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let outcome = restore_repo_path(&runtime, file, &repo, &spec)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::Export { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot export while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let exported = export_repo_path(&runtime, file, &repo, &spec)?;
                Ok(Some(format!(
                    "Exported {exported} to {}",
                    spec.output.display()
                )))
            }
            GraftPragma::JsonExport { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot export while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let exported = export_repo_path(&runtime, file, &repo, &spec)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonExportOutcome {
                    operation: "export",
                    current_head,
                    current_branch,
                    source: spec.source,
                    path: exported,
                    kind: repo_tracked_path_kind_json_label(RepoTrackedPathKind::SqliteDatabase),
                    output: spec.output.display().to_string(),
                })?))
            }

            GraftPragma::VolumeSwitch { vid, local, remote } => {
                if !file.is_idle() {
                    return pragma_err!("cannot switch while there is an open transaction");
                }

                let volume = runtime.volume_open(Some(vid), local, remote)?;
                file.switch_volume(&volume.vid)?;

                Ok(Some(format!(
                    "Switched to Volume {} with local Log {} and remote Log {}",
                    volume.vid, volume.local, volume.remote,
                )))
            }

            GraftPragma::VolumeInfo => Ok(Some(format_volume_info(&runtime, file)?)),
            GraftPragma::Status { spec } => {
                let repo = repo_for_file(file)?;
                let status = repo_status_for_file(&runtime, file, &repo)?;
                let status = filter_repo_status_by_kind(status, spec.kind);
                Ok(Some(format_repo_status(&status)?))
            }
            GraftPragma::VolumeStatus => Ok(Some(format_volume_status(&runtime, file)?)),

            GraftPragma::RepoInit => {
                let outcome = run_repo_init(file)?;
                Ok(Some(format_repo_init_outcome(&outcome)))
            }
            GraftPragma::JsonRepoInit => {
                let outcome = run_repo_init(file)?;
                Ok(Some(to_json(&JsonInitOutcome {
                    operation: "init",
                    current_head: outcome.current_head,
                    current_branch: outcome.current_branch,
                    graft_dir: outcome.graft_dir.display().to_string(),
                    worktree: outcome.worktree.display().to_string(),
                    path: outcome.path,
                    kind: repo_tracked_path_kind_json_label(RepoTrackedPathKind::SqliteDatabase),
                    preserved_contents: outcome.preserved_contents,
                })?))
            }

            GraftPragma::RepoClone { spec } => {
                let outcome = run_repo_clone(file, spec)?;
                Ok(Some(format!(
                    "Cloned origin/{} at {} into {}",
                    outcome.branch,
                    &outcome.head[..outcome.head.len().min(12)],
                    outcome.graft_dir.display()
                )))
            }
            GraftPragma::JsonRepoClone { spec } => {
                let outcome = run_repo_clone(file, spec)?;
                Ok(Some(to_json(&JsonCloneOutcome {
                    operation: "clone",
                    current_head: outcome.current_head,
                    current_branch: outcome.current_branch,
                    remote: json_remote_info(outcome.remote),
                    branch: outcome.branch,
                    head: outcome.head,
                    commits: outcome.commits,
                    graft_dir: outcome.graft_dir.display().to_string(),
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::JsonStatus { spec } => {
                let repo = repo_for_file(file)?;
                let status = repo_status_for_file(&runtime, file, &repo)?;
                let status = filter_repo_status_by_kind(status, spec.kind);
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                let current_head = status.head_target.clone();
                let current_branch = repo.current_branch()?;
                let conflict_analysis =
                    current_file_status_row_merge_analysis_lossy(&runtime, file, &repo, None);
                Ok(Some(to_json(&JsonRepoStatus {
                    current_head,
                    current_branch,
                    kind,
                    status,
                    conflict_analysis,
                })?))
            }

            GraftPragma::Add { spec } => {
                let entries = run_repo_add(&runtime, file, &spec)?;
                Ok(Some(format_added_entries(&entries)))
            }
            GraftPragma::JsonAdd { spec } => {
                let entries = run_repo_add(&runtime, file, &spec)?;
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                Ok(Some(to_json(&JsonAddOutcome {
                    operation: "add",
                    current_head,
                    current_branch,
                    kind,
                    paths: json_staged_entry_paths(&repo, &entries)?,
                })?))
            }

            GraftPragma::Remove { spec } => {
                let paths = run_repo_remove(&runtime, file, &spec)?;
                Ok(Some(format_removed_paths(&paths)))
            }
            GraftPragma::JsonRemove { spec } => {
                let paths = run_repo_remove(&runtime, file, &spec)?;
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonRemoveOutcome {
                    operation: "rm",
                    current_head,
                    current_branch,
                    cached: spec.cached,
                    paths,
                })?))
            }

            GraftPragma::Commit { message } => {
                let outcome = run_repo_commit(&runtime, file, message)?;
                let commit = outcome.commit;
                Ok(Some(format!("[{}] {}", &commit.id[..12], commit.message)))
            }
            GraftPragma::JsonCommit { message } => {
                let outcome = run_repo_commit(&runtime, file, message)?;
                let head = outcome.commit.id.clone();
                let paths = json_commit_path_changes(&outcome.commit);
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonCommitOutcome {
                    operation: "commit",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    paths,
                    commit: json_commit_summary(outcome.commit),
                })?))
            }

            GraftPragma::Branch { mode } => {
                let repo = repo_for_file(file)?;
                let branches = repo.branches()?;
                let remote_branches = if mode.includes_remote() {
                    repo.remote_tracking_branches()?
                } else {
                    Vec::new()
                };
                Ok(Some(format_branches(&branches, &remote_branches, mode)?))
            }
            GraftPragma::JsonBranch { mode } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let branches = repo.branches()?;
                let remote_branches = if mode.includes_remote() {
                    repo.remote_tracking_branches()?
                } else {
                    Vec::new()
                };
                Ok(Some(to_json(&JsonBranchList {
                    current_head,
                    current_branch,
                    branches,
                    remote_branches,
                })?))
            }

            GraftPragma::BranchCreate { name, start_point } => {
                let branch = run_repo_branch_create(file, name, start_point)?;
                Ok(Some(format_branch_created(&branch)))
            }
            GraftPragma::JsonBranchCreate { name, start_point } => {
                let branch = run_repo_branch_create(file, name, start_point)?;
                let outcome = json_branch_mutation_outcome(file, "branch_create", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::BranchDelete { name, force } => {
                let branch = run_repo_branch_delete(file, name, force)?;
                Ok(Some(format_branch_deleted(&branch, force)))
            }
            GraftPragma::JsonBranchDelete { name, force } => {
                let branch = run_repo_branch_delete(file, name, force)?;
                let outcome = json_branch_mutation_outcome(file, "branch_delete", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::BranchRename { old, new, force } => {
                let (old, branch) = run_repo_branch_rename(file, old, new, force)?;
                Ok(Some(format_branch_renamed(&old, &branch, force)))
            }
            GraftPragma::JsonBranchRename { old, new, force } => {
                let (old, branch) = run_repo_branch_rename(file, old, new, force)?;
                let outcome =
                    json_branch_mutation_outcome(file, "branch_rename", branch, Some(old))?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::BranchUpstream { branch, remote, remote_branch } => {
                let branch = run_repo_branch_upstream(file, branch, remote, remote_branch)?;
                Ok(Some(format_branch_upstream(&branch)))
            }
            GraftPragma::JsonBranchUpstream { branch, remote, remote_branch } => {
                let branch = run_repo_branch_upstream(file, branch, remote, remote_branch)?;
                let outcome = json_branch_mutation_outcome(file, "branch_upstream", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::BranchUnsetUpstream { branch } => {
                let branch = run_repo_branch_unset_upstream(file, branch)?;
                Ok(Some(format_branch_upstream_unset(&branch)))
            }
            GraftPragma::JsonBranchUnsetUpstream { branch } => {
                let branch = run_repo_branch_unset_upstream(file, branch)?;
                let outcome =
                    json_branch_mutation_outcome(file, "branch_unset_upstream", branch, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::TagCreate { name, target, message } => {
                let tag = run_repo_tag_create(file, name, target, message)?;
                Ok(Some(format_tag_created(&tag)))
            }
            GraftPragma::JsonTagCreate { name, target, message } => {
                let tag = run_repo_tag_create(file, name, target, message)?;
                let outcome = json_tag_mutation_outcome(file, "tag_create", tag)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::TagDelete { name } => {
                let tag = run_repo_tag_delete(file, name)?;
                Ok(Some(format_tag_deleted(&tag)))
            }
            GraftPragma::JsonTagDelete { name } => {
                let tag = run_repo_tag_delete(file, name)?;
                let outcome = json_tag_mutation_outcome(file, "tag_delete", tag)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::SwitchBranch { name, force } => {
                run_repo_switch_branch(&runtime, file, name.clone(), force)?;
                Ok(Some(format!("Switched to branch '{name}'")))
            }
            GraftPragma::JsonSwitchBranch { name, force } => {
                let outcome = run_repo_switch_branch(&runtime, file, name, force)?;
                let head = outcome.target.clone();
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonSwitchOutcome {
                    operation: "switch_branch",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    target: outcome.target,
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::SwitchCreate { name, start_point, force } => {
                let outcome = run_repo_switch_create(&runtime, file, name, start_point, force)?;
                Ok(Some(format_branch_created(&outcome.branch)))
            }
            GraftPragma::JsonSwitchCreate { name, start_point, force } => {
                let outcome = run_repo_switch_create(&runtime, file, name, start_point, force)?;
                let head = outcome.branch.target.clone();
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonSwitchOutcome {
                    operation: "switch_create",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch.name,
                    target: outcome.branch.target,
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::Merge { rev } => {
                let outcome = run_repo_merge(&runtime, file, &rev)?;
                let repo = repo_for_file(file)?;
                Ok(Some(format_merge_outcome_with_row_auto_merge(
                    &runtime,
                    file,
                    &repo,
                    &outcome.outcome,
                    outcome.row_auto_merge.as_ref(),
                    None,
                )?))
            }
            GraftPragma::JsonMerge { rev } => {
                let outcome = run_repo_merge(&runtime, file, &rev)?;
                let repo = repo_for_file(file)?;
                let conflict_analysis =
                    current_file_status_row_merge_analysis_lossy(&runtime, file, &repo, None);
                let head = merge_fast_forward_head(&outcome.outcome);
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonMergeCommandOutcome {
                    operation: "merge",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    outcome: outcome.outcome,
                    paths: outcome.paths,
                    conflict_analysis,
                })?))
            }

            GraftPragma::MergeAbort => {
                let outcome = run_repo_merge_abort(&runtime, file)?;
                Ok(Some(format!(
                    "Aborted merge; reset HEAD to {}",
                    &outcome.target[..outcome.target.len().min(12)]
                )))
            }
            GraftPragma::JsonMergeAbort => {
                let outcome = run_repo_merge_abort(&runtime, file)?;
                let head = outcome.target.clone();
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonMergeAbortCommandOutcome {
                    operation: "merge_abort",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    target: outcome.target,
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::MergeContinue { message } => {
                let outcome = run_repo_merge_continue(&runtime, file, message)?;
                let commit = outcome.commit;
                Ok(Some(format!(
                    "Merge commit [{}] {}",
                    &commit.id[..commit.id.len().min(12)],
                    commit.message
                )))
            }
            GraftPragma::JsonMergeContinue { message } => {
                let outcome = run_repo_merge_continue(&runtime, file, message)?;
                let head = outcome.commit.id.clone();
                let paths = json_commit_path_changes(&outcome.commit);
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonMergeContinueCommandOutcome {
                    operation: "merge_continue",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    paths,
                    commit: json_commit_summary(outcome.commit),
                })?))
            }

            GraftPragma::Conflicts => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_conflicts(&repo.status()?)?))
            }

            GraftPragma::JsonConflicts => {
                let repo = repo_for_file(file)?;
                let remote = repo_default_remote_store(&repo);
                Ok(Some(to_json(&repo_conflict_artifacts(
                    &runtime, &repo, remote,
                )?)?))
            }

            GraftPragma::Resolve { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot resolve while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let side = spec.side;
                let outcome = resolve_repo_conflict_for_file(&runtime, file, &repo, spec)?;
                Ok(Some(format!(
                    "Resolved {} using {}",
                    outcome.path,
                    side.label()
                )))
            }

            GraftPragma::JsonResolveConflict { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot resolve while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let side = spec.side;
                let outcome = resolve_repo_conflict_for_file(&runtime, file, &repo, spec)?;
                let remote = repo_default_remote_store(&repo);
                let remaining_conflicts =
                    unresolved_conflict_artifact_count(&runtime, &repo, remote)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonResolveConflictOutcome {
                    operation: "resolve_conflict",
                    current_head,
                    current_branch,
                    path: outcome.path,
                    path_kind: repo_tracked_path_kind_json_label(outcome.path_kind),
                    storage: repo_path_storage_json_label(outcome.path_storage),
                    resolution: side.label(),
                    remaining_conflicts,
                })?))
            }

            GraftPragma::RemoteAdd { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_add(&name, config)?;
                Ok(Some(format_remote(&remote)))
            }
            GraftPragma::JsonRemoteAdd { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_add(&name, config)?;
                let outcome = json_remote_mutation_outcome(file, "remote_add", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemoteRemove { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_remove(&name)?;
                Ok(Some(format!("Removed remote '{}'", remote.name)))
            }
            GraftPragma::JsonRemoteRemove { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_remove(&name)?;
                let outcome = json_remote_mutation_outcome(file, "remote_remove", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemoteRename { old, new } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_rename(&old, &new)?;
                Ok(Some(format!(
                    "Renamed remote '{}' to '{}': {}",
                    old,
                    remote.name,
                    remote_config_uri(&remote.config)
                )))
            }
            GraftPragma::JsonRemoteRename { old, new } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_rename(&old, &new)?;
                let outcome =
                    json_remote_mutation_outcome(file, "remote_rename", remote, Some(old))?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemoteGetUrl { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_get_url(&name)?;
                Ok(Some(remote_config_uri(&remote.config)))
            }
            GraftPragma::JsonRemoteGetUrl { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_get_url(&name)?;
                let outcome = json_remote_mutation_outcome(file, "remote_get_url", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemoteSetUrl { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_set_url(&name, config)?;
                Ok(Some(format!(
                    "Updated remote '{}': {}",
                    remote.name,
                    remote_config_uri(&remote.config)
                )))
            }
            GraftPragma::JsonRemoteSetUrl { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_set_url(&name, config)?;
                let outcome = json_remote_mutation_outcome(file, "remote_set_url", remote, None)?;
                Ok(Some(to_json(&outcome)?))
            }

            GraftPragma::RemotePrune { name } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.remote_prune(&name)?;
                Ok(Some(format_remote_prune_outcome(&outcome)?))
            }
            GraftPragma::JsonRemotePrune { name } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.remote_prune(&name)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonRemotePruneCommandOutcome {
                    operation: "remote_prune",
                    current_head,
                    current_branch,
                    outcome,
                })?))
            }

            GraftPragma::LsRemote { name } => {
                let repo = repo_for_file(file)?;
                let default_branch = repo.remote_default_branch(&name)?;
                let refs = repo.remote_branch_refs(&name)?;
                Ok(Some(format_ls_remote(
                    &name,
                    default_branch.as_deref(),
                    &refs,
                )?))
            }
            GraftPragma::JsonLsRemote { name } => {
                let repo = repo_for_file(file)?;
                let default_branch = repo.remote_default_branch(&name)?;
                let refs = repo.remote_branch_refs(&name)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonLsRemoteOutcome {
                    operation: "ls_remote",
                    current_head,
                    current_branch,
                    remote: name,
                    default_branch,
                    refs,
                })?))
            }

            GraftPragma::Remotes => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_remotes(&repo.remotes()?)?))
            }
            GraftPragma::JsonRemotes => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonRemoteList {
                    current_head,
                    current_branch,
                    remotes: repo.remotes()?.into_iter().map(json_remote_info).collect(),
                })?))
            }

            GraftPragma::VolumeSnapshot => {
                let snapshot = file.snapshot_or_latest()?;
                Ok(Some(format!("{snapshot:?}")))
            }

            GraftPragma::Fetch { remote, branch, refspec, all } => {
                let repo = repo_for_file(file)?;
                Ok(Some(run_repo_fetch(&repo, remote, branch, refspec, all)?))
            }
            GraftPragma::JsonFetch { remote, branch, refspec, all } => {
                let repo = repo_for_file(file)?;
                Ok(Some(run_repo_fetch_json(
                    &repo, remote, branch, refspec, all,
                )?))
            }
            GraftPragma::FetchAsync { remote, branch, refspec, all } => {
                repo_for_file(file)?;
                let id = async_jobs().spawn_fetch(
                    PathBuf::from(file.tag.clone()),
                    remote,
                    branch,
                    refspec,
                    all,
                    AsyncJobResultFormat::Text,
                );
                Ok(Some(id))
            }
            GraftPragma::JsonFetchAsync { remote, branch, refspec, all, mode } => {
                repo_for_file(file)?;
                let id = async_jobs().spawn_fetch(
                    PathBuf::from(file.tag.clone()),
                    remote,
                    branch,
                    refspec,
                    all,
                    AsyncJobResultFormat::Json,
                );
                match mode {
                    JsonFetchAsyncMode::LegacyId => Ok(Some(id)),
                    JsonFetchAsyncMode::WithStatus => Ok(Some(async_jobs().json_status(&id)?)),
                }
            }
            GraftPragma::JobStatus { id } => Ok(Some(async_jobs().status_json(&id)?)),
            GraftPragma::JsonJobStatus { id } => Ok(Some(async_jobs().json_status(&id)?)),
            GraftPragma::JobResult { id } => Ok(Some(async_jobs().result(&id)?)),
            GraftPragma::JsonJobResult { id } => Ok(Some(async_jobs().result(&id)?)),
            GraftPragma::Pull { remote, branch, refspec, all } => {
                let outcome = run_repo_pull(&runtime, file, remote, branch, refspec, all)?;
                let repo = repo_for_file(file)?;
                let checkout_remote = Arc::new(repo.remote_store(&outcome.outcome.remote)?);
                Ok(Some(format_pull_outcome_with_row_analysis(
                    &runtime,
                    file,
                    &repo,
                    &outcome.outcome,
                    Some(checkout_remote),
                )?))
            }
            GraftPragma::JsonPull { remote, branch, refspec, all } => {
                let outcome = run_repo_pull(&runtime, file, remote, branch, refspec, all)?;
                let repo = repo_for_file(file)?;
                let remote = repo
                    .remote_store(&outcome.outcome.remote)
                    .ok()
                    .map(Arc::new);
                let conflict_analysis =
                    current_file_status_row_merge_analysis_lossy(&runtime, file, &repo, remote);
                Ok(Some(to_json(&JsonPullCommandOutcome {
                    operation: "pull",
                    current_head: outcome.current_head,
                    current_branch: outcome.current_branch,
                    outcome: outcome.outcome,
                    paths: outcome.paths,
                    conflict_analysis,
                })?))
            }

            GraftPragma::Push { remote, branch, refspec, all, force } => {
                let repo = repo_for_file(file)?;
                let outcome = run_repo_push(&runtime, &repo, remote, branch, refspec, all, force)?;
                Ok(Some(format_push_command_outcome(&outcome)?))
            }
            GraftPragma::JsonPush { remote, branch, refspec, all, force } => {
                let repo = repo_for_file(file)?;
                let outcome = run_repo_push(&runtime, &repo, remote, branch, refspec, all, force)?;
                Ok(Some(to_json(&json_push_command_outcome(&repo, &outcome)?)?))
            }
            GraftPragma::VolumeFetch => Ok(Some(fetch_or_pull(&runtime, file, false)?)),
            GraftPragma::VolumePull => Ok(Some(fetch_or_pull(&runtime, file, true)?)),
            GraftPragma::VolumePush => Ok(Some(push(&runtime, file)?)),

            GraftPragma::VolumeAudit => Ok(Some(format_volume_audit(&runtime, file)?)),
            GraftPragma::VolumeJsonAudit => Ok(Some(to_json(&json_volume_audit(&runtime, file)?)?)),
            GraftPragma::RepoAudit { spec } => {
                let repo = repo_for_file(file)?;
                if spec.repair {
                    let remote = repo_default_remote(&repo, spec.remote.clone())?;
                    let outcome = repo.repair_artifacts_from_remote(&remote)?;
                    Ok(Some(format_repo_artifact_repair(&outcome)?))
                } else {
                    Ok(Some(format_repo_artifact_audit(&repo.audit_artifacts()?)?))
                }
            }
            GraftPragma::JsonRepoAudit { spec } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                if spec.repair {
                    let remote = repo_default_remote(&repo, spec.remote.clone())?;
                    let outcome = repo.repair_artifacts_from_remote(&remote)?;
                    Ok(Some(to_json(&JsonRepoArtifactRepair {
                        operation: "audit_repair",
                        current_head,
                        current_branch,
                        outcome,
                    })?))
                } else {
                    Ok(Some(to_json(&JsonRepoArtifactAudit {
                        current_head,
                        current_branch,
                        audit: repo.audit_artifacts()?,
                    })?))
                }
            }
            GraftPragma::LargeFileFetch { spec } => {
                let repo = repo_for_file(file)?;
                let remote = repo_default_remote(&repo, spec.remote.clone())?;
                let outcome = repo.fetch_large_file_payloads(&remote, spec.rev.as_deref())?;
                Ok(Some(format_large_file_fetch_outcome(&outcome)?))
            }
            GraftPragma::JsonLargeFileFetch { spec, operation } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let remote = repo_default_remote(&repo, spec.remote.clone())?;
                let outcome = repo.fetch_large_file_payloads(&remote, spec.rev.as_deref())?;
                Ok(Some(to_json(&JsonLargeFileFetchOutcome {
                    operation,
                    current_head,
                    current_branch,
                    outcome,
                })?))
            }
            GraftPragma::LargeFileStatus { spec } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.large_file_payloads_status(spec.rev.as_deref())?;
                Ok(Some(format_large_file_status_outcome(&outcome)?))
            }
            GraftPragma::JsonLargeFileStatus { spec, operation } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let outcome = repo.large_file_payloads_status(spec.rev.as_deref())?;
                Ok(Some(to_json(&JsonLargeFileStatusOutcome {
                    operation,
                    current_head,
                    current_branch,
                    outcome,
                })?))
            }
            GraftPragma::LargeFilePrune { spec } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.prune_large_file_payloads(spec.dry_run)?;
                Ok(Some(format_large_file_prune_outcome(&outcome)?))
            }
            GraftPragma::JsonLargeFilePrune { spec, operation } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let outcome = repo.prune_large_file_payloads(spec.dry_run)?;
                Ok(Some(to_json(&JsonLargeFilePruneOutcome {
                    operation,
                    current_head,
                    current_branch,
                    outcome,
                })?))
            }
            GraftPragma::LsFiles { spec } => {
                let repo = repo_for_file(file)?;
                if spec.others {
                    let paths = filter_tracked_paths_by_kind(repo.untracked_paths()?, spec.kind);
                    Ok(Some(format_repo_untracked_paths(&paths)?))
                } else if spec.stage {
                    let paths = filter_tracked_path_entries_by_kind(
                        repo.tracked_path_entries()?,
                        spec.kind,
                    );
                    Ok(Some(format_repo_tracked_path_entries(&paths)?))
                } else if spec.details {
                    let paths = filter_tracked_path_details_by_kind(
                        repo.tracked_path_details()?,
                        spec.kind,
                    );
                    Ok(Some(format_repo_tracked_path_details(&paths)?))
                } else {
                    let paths = filter_tracked_paths_by_kind(repo.tracked_paths()?, spec.kind);
                    Ok(Some(format_repo_tracked_paths(&paths)?))
                }
            }
            GraftPragma::JsonLsFiles { spec } => {
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                if spec.others {
                    let paths = filter_tracked_paths_by_kind(repo.untracked_paths()?, spec.kind);
                    Ok(Some(to_json(&JsonLsFilesOutcome {
                        current_head,
                        current_branch,
                        stage: spec.stage,
                        details: spec.details,
                        others: spec.others,
                        kind,
                        paths,
                    })?))
                } else if spec.stage {
                    let paths = filter_tracked_path_entries_by_kind(
                        repo.tracked_path_entries()?,
                        spec.kind,
                    );
                    Ok(Some(to_json(&JsonLsFilesOutcome {
                        current_head,
                        current_branch,
                        stage: spec.stage,
                        details: spec.details,
                        others: spec.others,
                        kind,
                        paths,
                    })?))
                } else if spec.details {
                    let paths = filter_tracked_path_details_by_kind(
                        repo.tracked_path_details()?,
                        spec.kind,
                    );
                    Ok(Some(to_json(&JsonLsFilesOutcome {
                        current_head,
                        current_branch,
                        stage: spec.stage,
                        details: spec.details,
                        others: spec.others,
                        kind,
                        paths,
                    })?))
                } else {
                    let paths = filter_tracked_paths_by_kind(repo.tracked_paths()?, spec.kind);
                    Ok(Some(to_json(&JsonLsFilesOutcome {
                        current_head,
                        current_branch,
                        stage: spec.stage,
                        details: spec.details,
                        others: spec.others,
                        kind,
                        paths,
                    })?))
                }
            }
            GraftPragma::ConfigGet { key } => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entry(&repo.config_get(&key)?)?))
            }
            GraftPragma::JsonConfigGet { key } => {
                let repo = repo_for_file(file)?;
                let entry = repo.config_get(&key)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonConfigEntryOutcome {
                    current_head,
                    current_branch,
                    entry,
                })?))
            }
            GraftPragma::ConfigList => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entries(&repo.config_list()?)?))
            }
            GraftPragma::JsonConfigList { mode } => {
                let repo = repo_for_file(file)?;
                let entries = repo.config_list()?;
                match mode {
                    JsonConfigListMode::LegacyArray => Ok(Some(to_json(&entries)?)),
                    JsonConfigListMode::WithStatus => {
                        let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                        Ok(Some(to_json(&JsonConfigListOutcome {
                            current_head,
                            current_branch,
                            entries,
                        })?))
                    }
                }
            }
            GraftPragma::ConfigSet { key, value } => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entry(
                    &repo.config_set(&key, &value)?,
                )?))
            }
            GraftPragma::JsonConfigSet { key, value } => {
                let repo = repo_for_file(file)?;
                let entry = repo.config_set(&key, &value)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonConfigMutationOutcome {
                    operation: "config_set",
                    current_head,
                    current_branch,
                    entry,
                })?))
            }
            GraftPragma::ConfigUnset { key } => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_config_entry(&repo.config_unset(&key)?)?))
            }
            GraftPragma::JsonConfigUnset { key } => {
                let repo = repo_for_file(file)?;
                let entry = repo.config_unset(&key)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonConfigMutationOutcome {
                    operation: "config_unset",
                    current_head,
                    current_branch,
                    entry,
                })?))
            }

            GraftPragma::VolumeHydrate => {
                let snapshot = file.snapshot_or_latest()?;
                runtime.snapshot_hydrate(snapshot)?;
                Ok(None)
            }

            GraftPragma::Version => {
                const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
                const GITHUB_SHA: Option<&str> = option_env!("GITHUB_SHA");
                let mut out = format!("Graft Version: {PKG_VERSION}");
                if let Some(sha) = GITHUB_SHA {
                    writeln!(&mut out, "\nGit Commit: {sha}")?;
                }
                Ok(Some(out))
            }

            GraftPragma::VolumeImport => {
                pragma_err!(
                    "deprecated: use `vacuum into` instead: https://graft.rs/r/graft_import"
                )
            }

            GraftPragma::VolumeExport(path) => volume_export(&runtime, file, path).map(Some),

            GraftPragma::VolumeDumpSqliteHeader => {
                let reader = runtime.volume_reader(file.vid.clone())?;
                let page = reader.read_page(PageIdx::FIRST)?;
                let header = SqliteHeader::read_from_bytes(&page[..100])
                    .expect("failed to parse SQLite header");
                Ok(Some(format!("{header:#?}")))
            }

            GraftPragma::VolumeDumpCommit { logref } => {
                format_debug_show_lsn(&runtime, &logref).map(Some)
            }

            GraftPragma::DebugLogLsn => format_debug_log_lsn(&runtime, file).map(Some),

            GraftPragma::DebugShowLsn { logref } => {
                format_debug_show_lsn(&runtime, &logref).map(Some)
            }

            GraftPragma::DebugDiffLsn { from, to } => {
                if from.log != to.log {
                    return pragma_err!("debug LSN diff requires both refs to use the same log");
                }
                let diff = runtime.diff_commits(&from.log, from.lsn, to.lsn)?;
                Ok(Some(format_debug_page_diff(&diff)))
            }

            GraftPragma::Log => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_repo_log(&repo)?))
            }

            GraftPragma::VolumeCheckoutLsn { lsn } => {
                if !file.is_idle() {
                    return pragma_err!("cannot checkout while there is an open transaction");
                }

                let new_volume = runtime.volume_checkout(&file.vid, lsn)?;
                file.switch_volume(&new_volume.vid)?;

                Ok(Some(format!(
                    "Checked out LSN {} into new Volume {} (local log: {})",
                    lsn, new_volume.vid, new_volume.local
                )))
            }

            GraftPragma::VolumeResetTo { lsn } => {
                if !file.is_idle() {
                    return pragma_err!("cannot reset while there is an open transaction");
                }

                let tag = file.tag.clone();
                let new_volume = runtime.volume_reset_to(&tag, lsn)?;
                file.switch_volume(&new_volume.vid)?;

                Ok(Some(format!(
                    "Reset tag '{}' to LSN {} (new Volume: {}, local log: {})",
                    tag, lsn, new_volume.vid, new_volume.local
                )))
            }

            GraftPragma::Reset { rev, mode } => {
                let outcome = run_repo_reset(&runtime, file, &rev, mode)?;

                Ok(Some(format!(
                    "Reset HEAD to {} ({})",
                    &outcome.outcome.target[..outcome.outcome.target.len().min(12)],
                    reset_mode_label(mode)
                )))
            }
            GraftPragma::JsonReset { rev, mode } => {
                let outcome = run_repo_reset(&runtime, file, &rev, mode)?;
                let head = outcome.outcome.target.clone();
                let repo = repo_for_file(file)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonResetCommandOutcome {
                    operation: "reset",
                    current_head,
                    current_branch,
                    head,
                    branch: outcome.branch,
                    outcome: outcome.outcome,
                    paths: outcome.paths,
                })?))
            }

            GraftPragma::VolumeDiff { from, to, mode } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                match mode {
                    DiffMode::Default => {
                        // Built-in table-level diff using our B-tree parser
                        let report =
                            crate::sql_diff::generate_diff_report(&runtime, file, from, to)?;
                        Ok(Some(report))
                    }
                    DiffMode::Rows => {
                        // Row-level detailed diff
                        row_diff_impl(&runtime, file, from, to)
                    }
                }
            }

            GraftPragma::RepoDiff { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                let mode = spec.mode;
                let repo = repo_for_file(file)?;
                let diff = repo_diff_for_spec(&runtime, file, &repo, spec)?;
                match mode {
                    DiffMode::Default => Ok(Some(format_repo_diff(&diff)?)),
                    DiffMode::Rows => Ok(Some(format_repo_row_diff(&runtime, &repo, &diff)?)),
                }
            }

            GraftPragma::Show { target } => {
                if !file.is_idle() {
                    return pragma_err!("cannot show while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let commit = repo.show_revision(&target)?;
                Ok(Some(format_repo_show(&commit)?))
            }

            GraftPragma::JsonLog { mode } => {
                let repo = repo_for_file(file)?;
                let commits = repo.log()?;
                match mode {
                    JsonLogMode::LegacyArray => Ok(Some(to_json(&commits)?)),
                    JsonLogMode::WithStatus => {
                        let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                        Ok(Some(to_json(&JsonRepoLogOutcome {
                            current_head,
                            current_branch,
                            commits,
                        })?))
                    }
                }
            }

            GraftPragma::VolumeJsonDiff { from, to, mode } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                match mode {
                    DiffMode::Default => {
                        let diff =
                            crate::row_level_diff::row_level_diff(&runtime, &file.vid, from, to)
                                .map_err(|e| {
                                    ErrCtx::PragmaErr(format!("Diff error: {e:?}").into())
                                })?;
                        let tables: Vec<crate::json::JsonTableSummary> = diff
                            .table_changes
                            .iter()
                            .map(|t| {
                                let (inserts, deletes, updates) = count_changes_json(&t.changes);
                                crate::json::JsonTableSummary {
                                    name: t.table_name.clone(),
                                    inserts,
                                    deletes,
                                    updates,
                                }
                            })
                            .collect();
                        let result = crate::json::JsonDiffResult {
                            from_lsn: from.to_u64(),
                            to_lsn: to.to_u64(),
                            logical_status: diff.logical_status().as_str().to_string(),
                            capabilities: json_diff_capabilities(&diff),
                            limitations: json_diff_limitations(&diff),
                            tables,
                            opaque_changes: json_opaque_changes(&diff.opaque_changes),
                        };
                        Ok(Some(serde_json::to_string(&result).map_err(|e| {
                            ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                        })?))
                    }
                    DiffMode::Rows => {
                        let diff =
                            crate::row_level_diff::row_level_diff(&runtime, &file.vid, from, to)
                                .map_err(|e| {
                                    ErrCtx::PragmaErr(format!("Diff error: {e:?}").into())
                                })?;
                        let tables: Vec<crate::json::JsonTableChanges> = diff
                            .table_changes
                            .iter()
                            .map(|t| {
                                let changes: Vec<crate::json::JsonRowChange> = t
                                    .changes
                                    .iter()
                                    .map(|c| match c {
                                        crate::row_level_diff::RowChange::Insert { rowid, row } => {
                                            crate::json::JsonRowChange {
                                                op: "insert".into(),
                                                rowid: *rowid,
                                                values: row
                                                    .values
                                                    .iter()
                                                    .map(crate::json::JsonRowChange::value_to_json)
                                                    .collect(),
                                                old_values: None,
                                            }
                                        }
                                        crate::row_level_diff::RowChange::Delete { rowid, row } => {
                                            crate::json::JsonRowChange {
                                                op: "delete".into(),
                                                rowid: *rowid,
                                                values: row
                                                    .values
                                                    .iter()
                                                    .map(crate::json::JsonRowChange::value_to_json)
                                                    .collect(),
                                                old_values: None,
                                            }
                                        }
                                        crate::row_level_diff::RowChange::Update {
                                            rowid,
                                            old_row,
                                            new_row,
                                        } => crate::json::JsonRowChange {
                                            op: "update".into(),
                                            rowid: *rowid,
                                            values: new_row
                                                .values
                                                .iter()
                                                .map(crate::json::JsonRowChange::value_to_json)
                                                .collect(),
                                            old_values: Some(
                                                old_row
                                                    .values
                                                    .iter()
                                                    .map(crate::json::JsonRowChange::value_to_json)
                                                    .collect(),
                                            ),
                                        },
                                    })
                                    .collect();
                                crate::json::JsonTableChanges {
                                    name: t.table_name.clone(),
                                    columns: t.columns.clone(),
                                    changes,
                                }
                            })
                            .collect();
                        let result = crate::json::JsonRowDiffResult {
                            from_lsn: from.to_u64(),
                            to_lsn: to.to_u64(),
                            logical_status: diff.logical_status().as_str().to_string(),
                            capabilities: json_diff_capabilities(&diff),
                            limitations: json_diff_limitations(&diff),
                            tables,
                            opaque_changes: json_opaque_changes(&diff.opaque_changes),
                        };
                        Ok(Some(serde_json::to_string(&result).map_err(|e| {
                            ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                        })?))
                    }
                }
            }

            GraftPragma::JsonRepoDiff { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot diff while there is an open transaction");
                }
                let mode = spec.mode;
                let kind = spec.kind.map(repo_tracked_path_kind_json_label);
                let repo = repo_for_file(file)?;
                let diff = repo_diff_for_spec(&runtime, file, &repo, spec)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                match mode {
                    DiffMode::Default => Ok(Some(to_json(&JsonRepoDiffOutcome {
                        current_head,
                        current_branch,
                        kind,
                        diff,
                    })?)),
                    DiffMode::Rows => {
                        let rows = json_repo_row_diff(&runtime, &repo, &diff)?;
                        Ok(Some(to_json(&JsonRepoDiffOutcome {
                            current_head,
                            current_branch,
                            kind,
                            diff: rows,
                        })?))
                    }
                }
            }

            GraftPragma::JsonShow { target } => {
                if !file.is_idle() {
                    return pragma_err!("cannot show while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let commit = repo.show_revision(&target)?;
                let (current_head, current_branch) = repo_head_and_branch(&repo)?;
                Ok(Some(to_json(&JsonRepoShowOutcome {
                    current_head,
                    current_branch,
                    commit,
                })?))
            }

            GraftPragma::VolumeJsonInfo => {
                let result = json_volume_info(&runtime, file)?;
                Ok(Some(serde_json::to_string(&result).map_err(|e| {
                    ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                })?))
            }

            GraftPragma::VolumeTableLog { table } => {
                let entries = table_log_entries(&runtime, &file.vid, &table)?;
                if entries.is_empty() {
                    return Ok(Some(format!("No changes found for table '{table}'.")));
                }
                let mut f = String::new();
                writeln!(&mut f, "Changes for table '{table}':")?;
                writeln!(
                    &mut f,
                    "{:<6} {:<20} {:<10} DETAIL",
                    "LSN", "WHEN", "CHANGES"
                )?;
                writeln!(&mut f, "{}", "-".repeat(75))?;
                for e in entries {
                    writeln!(
                        &mut f,
                        "{:<6} {:<20} {:<10} {}",
                        e.lsn, e.when, e.summary, e.detail
                    )?;
                }
                Ok(Some(f))
            }

            GraftPragma::VolumeJsonTableLog { table } => {
                let entries = table_log_entries(&runtime, &file.vid, &table)?;
                let json_entries: Vec<crate::json::JsonTableLogEntry> = entries
                    .iter()
                    .map(|e| crate::json::JsonTableLogEntry {
                        lsn: e.lsn,
                        timestamp_ms: e.timestamp_ms,
                        summary: e.summary.clone(),
                        detail: e.detail.clone(),
                    })
                    .collect();
                Ok(Some(serde_json::to_string(&json_entries).map_err(|e| {
                    ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                })?))
            }

            GraftPragma::VolumeSetMessage { message } => {
                file.pending_message = Some(message.clone());
                Ok(Some(format!("Commit message set: '{message}'")))
            }
        }
    }
}

macro_rules! pluralize {
    ($n:expr, $s:literal) => {
        if $n == 1 { $s } else { concat!($s, "s") }
    };
}

fn run_repo_init(file: &mut VolFile) -> Result<RepoInitOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot initialize a repository while there is an open transaction");
    }
    let repo = Repository::init_for_file(&file.tag)?;
    let preserved_contents = file.attach_repo_preserving_contents(repo.clone())?;
    if preserved_contents {
        repo.mark_dirty_path(&file.tag)?;
    }
    let path = repo.file_key(&file.tag)?;
    let (current_head, current_branch) = repo_head_and_branch(&repo)?;
    Ok(RepoInitOutcome {
        graft_dir: repo.graft_dir().to_path_buf(),
        worktree: repo.worktree().to_path_buf(),
        path,
        preserved_contents,
        current_head,
        current_branch,
    })
}

fn format_repo_init_outcome(outcome: &RepoInitOutcome) -> String {
    format!(
        "Initialized empty Graft repository in {}",
        outcome.graft_dir.display()
    )
}

fn repo_for_file(file: &mut VolFile) -> Result<Repository, ErrCtx> {
    if let Some(repo) = &file.repo {
        return Ok(repo.clone());
    }

    if !should_discover_repo(&file.tag) {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::NotFound(PathBuf::from(
            &file.tag,
        ))));
    }
    let repo = Repository::discover_for_file(&file.tag)?;
    file.repo = Some(repo.clone());
    Ok(repo)
}

fn run_repo_clone(file: &mut VolFile, spec: RepoCloneSpec) -> Result<RepoCloneOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot clone while there is an open transaction");
    }
    if file.repo.is_some() || Repository::discover_for_file(&file.tag).is_ok() {
        return pragma_err!("cannot clone into an existing .graft repository");
    }

    let repo = Repository::init_for_file(&file.tag)?;
    let graft_dir = repo.graft_dir().to_path_buf();
    let mut attached = false;
    let result = (|| {
        let remote_info = repo.remote_add("origin", spec.config)?;
        let branch = match spec.branch {
            Some(branch) => branch,
            None => repo
                .remote_default_branch("origin")?
                .unwrap_or(repo.default_branch()?),
        };
        let fetch = repo.fetch("origin", &branch)?;
        repo.branch_create(&branch, Some(&format!("refs/remotes/origin/{branch}")))?;
        repo.set_branch_upstream(&branch, "origin", &branch)?;
        let plan = repo.plan_switch_branch(&branch)?;
        file.attach_repo(repo.clone())?;
        attached = true;
        let runtime = file.runtime().clone();
        let remote = Arc::new(repo.remote_store("origin")?);
        let plan = prepare_repo_checkout_plan(&runtime, &plan, Some(remote.clone()))?;
        let previous_files = BTreeMap::new();
        let previous_artifacts = BTreeMap::new();
        let paths = checkout_plan_path_actions(&plan, &previous_files, &previous_artifacts);
        repo.apply_switch_branch_plan(&branch, &plan)?;
        checkout_repo_plan(
            &runtime,
            file,
            &repo,
            &plan,
            &previous_files,
            &previous_artifacts,
            Some(remote),
        )?;
        let (current_head, current_branch) = repo_head_and_branch(&repo)?;
        Ok(RepoCloneOutcome {
            remote: remote_info,
            current_head,
            current_branch,
            branch: fetch.branch,
            head: fetch.head,
            commits: fetch.commits,
            graft_dir: graft_dir.clone(),
            paths,
        })
    })();
    if result.is_err() && !attached {
        let _ = std::fs::remove_dir_all(graft_dir);
    }
    result
}

fn run_repo_fetch(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
) -> Result<String, ErrCtx> {
    let outcome = run_repo_fetch_outcome(repo, remote, branch, refspec, all)?;
    format_fetch_command_outcome(&outcome)
}

fn run_repo_fetch_json(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
) -> Result<String, ErrCtx> {
    let outcome = run_repo_fetch_outcome(repo, remote, branch, refspec, all)?;
    to_json(&json_fetch_command_outcome(repo, &outcome)?)
}

fn run_repo_fetch_outcome(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
) -> Result<FetchCommandOutcome, ErrCtx> {
    if let Some(refspec) = refspec {
        let remote = repo_default_remote(repo, remote)?;
        let outcome = repo.fetch_refspec(&remote, &refspec)?;
        Ok(FetchCommandOutcome::Many(outcome))
    } else if all {
        let remote = repo_default_remote(repo, remote)?;
        let outcome = repo.fetch_all(&remote)?;
        Ok(FetchCommandOutcome::Many(outcome))
    } else {
        let upstream = repo_remote_branch(repo, remote, branch)?;
        let outcome = repo.fetch(&upstream.remote, &upstream.branch)?;
        Ok(FetchCommandOutcome::One(outcome))
    }
}

fn format_fetch_command_outcome(outcome: &FetchCommandOutcome) -> Result<String, ErrCtx> {
    match outcome {
        FetchCommandOutcome::One(outcome) => Ok(format!(
            "Fetched {}/{} at {} ({} new commits)",
            outcome.remote,
            outcome.branch,
            &outcome.head[..12],
            outcome.commits
        )),
        FetchCommandOutcome::Many(outcome) => format_fetch_all_outcome(outcome),
    }
}

fn json_fetch_command_outcome(
    repo: &Repository,
    outcome: &FetchCommandOutcome,
) -> Result<JsonFetchCommandOutcome, ErrCtx> {
    let (current_head, current_branch) = repo_head_and_branch(repo)?;
    Ok(JsonFetchCommandOutcome {
        operation: "fetch",
        current_head,
        current_branch,
        remote: outcome.remote(),
        branches: outcome.branches(),
        commits: outcome.commits(),
    })
}

fn run_repo_merge_abort(
    runtime: &Runtime,
    file: &mut VolFile,
) -> Result<RepoMergeAbortCommandOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot abort merge while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    let plan = repo.plan_merge_abort()?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    let paths = checkout_plan_path_actions(&plan.checkout, &previous_files, &previous_artifacts);
    let target = repo.apply_merge_abort_plan(&plan)?;
    checkout_repo_plan(
        runtime,
        file,
        &repo,
        &plan.checkout,
        &previous_files,
        &previous_artifacts,
        None,
    )?;
    clear_row_conflict_resolution_state(&repo)?;
    let branch = repo.current_branch()?;
    Ok(RepoMergeAbortCommandOutcome { target, branch, paths })
}

fn run_repo_merge_continue(
    runtime: &Runtime,
    file: &mut VolFile,
    message: String,
) -> Result<RepoCommitOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot continue merge while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    if repo.status()?.merge_head.is_none() {
        return pragma_err!("no merge in progress");
    }
    try_row_auto_merge_current_file_status_conflict(runtime, file, &repo, None)?;
    let tables = staged_commit_table_summary(runtime, &repo)?;
    let commit = repo.commit_staged_with_table_summary(message, tables)?;
    clear_row_conflict_resolution_state(&repo)?;
    let branch = repo.current_branch()?;
    Ok(RepoCommitOutcome { commit, branch })
}

fn run_repo_commit(
    runtime: &Runtime,
    file: &mut VolFile,
    message: String,
) -> Result<RepoCommitOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot commit while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    let tables = staged_commit_table_summary(runtime, &repo)?;
    let commit = repo.commit_staged_with_table_summary(message, tables)?;
    let branch = repo.current_branch()?;
    Ok(RepoCommitOutcome { commit, branch })
}

fn run_repo_branch_create(
    file: &mut VolFile,
    name: String,
    start_point: Option<String>,
) -> Result<BranchInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    if start_point.is_some() || repo.status()?.head_target.is_some() {
        repo.branch_create(&name, start_point.as_deref())
            .map_err(Into::into)
    } else {
        repo.branch_create_unborn(&name).map_err(Into::into)
    }
}

fn run_repo_branch_delete(
    file: &mut VolFile,
    name: String,
    force: bool,
) -> Result<BranchInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    repo.branch_delete(&name, force).map_err(Into::into)
}

fn run_repo_branch_rename(
    file: &mut VolFile,
    old: Option<String>,
    new: String,
    force: bool,
) -> Result<(String, BranchInfo), ErrCtx> {
    let repo = repo_for_file(file)?;
    let old = match old {
        Some(old) => old,
        None => repo.current_branch()?.ok_or_else(|| {
            ErrCtx::PragmaErr("cannot rename current branch in detached HEAD".into())
        })?,
    };
    let branch = repo.branch_rename(&old, &new, force)?;
    Ok((old, branch))
}

fn run_repo_branch_upstream(
    file: &mut VolFile,
    branch: Option<String>,
    remote: String,
    remote_branch: String,
) -> Result<BranchInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    let branch = current_or_named_branch(&repo, branch, "set upstream")?;
    repo.set_branch_upstream(&branch, &remote, &remote_branch)
        .map_err(Into::into)
}

fn run_repo_branch_unset_upstream(
    file: &mut VolFile,
    branch: Option<String>,
) -> Result<BranchInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    let branch = current_or_named_branch(&repo, branch, "unset upstream")?;
    repo.unset_branch_upstream(&branch).map_err(Into::into)
}

fn current_or_named_branch(
    repo: &Repository,
    branch: Option<String>,
    action: &'static str,
) -> Result<String, ErrCtx> {
    match branch {
        Some(branch) => Ok(branch),
        None => repo
            .current_branch()?
            .ok_or_else(|| ErrCtx::PragmaErr(format!("cannot {action} in detached HEAD").into())),
    }
}

fn json_branch_mutation_outcome(
    file: &mut VolFile,
    operation: &'static str,
    branch: BranchInfo,
    old_branch: Option<String>,
) -> Result<JsonBranchMutationOutcome, ErrCtx> {
    let repo = repo_for_file(file)?;
    let (current_head, current_branch) = repo_head_and_branch(&repo)?;
    Ok(JsonBranchMutationOutcome {
        operation,
        current_head,
        current_branch,
        branch,
        old_branch,
    })
}

fn json_tag_mutation_outcome(
    file: &mut VolFile,
    operation: &'static str,
    tag: TagInfo,
) -> Result<JsonTagMutationOutcome, ErrCtx> {
    let repo = repo_for_file(file)?;
    let (current_head, current_branch) = repo_head_and_branch(&repo)?;
    Ok(JsonTagMutationOutcome {
        operation,
        current_head,
        current_branch,
        tag,
    })
}

fn json_remote_mutation_outcome(
    file: &mut VolFile,
    operation: &'static str,
    remote: RemoteInfo,
    old_name: Option<String>,
) -> Result<JsonRemoteMutationOutcome, ErrCtx> {
    let repo = repo_for_file(file)?;
    let (current_head, current_branch) = repo_head_and_branch(&repo)?;
    Ok(JsonRemoteMutationOutcome {
        operation,
        current_head,
        current_branch,
        remote: json_remote_info(remote),
        old_name,
    })
}

fn run_repo_tag_create(
    file: &mut VolFile,
    name: String,
    target: Option<String>,
    message: Option<String>,
) -> Result<TagInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    match message {
        Some(message) => repo
            .tag_create_annotated(&name, target.as_deref(), message)
            .map_err(Into::into),
        None => repo
            .tag_create(&name, target.as_deref())
            .map_err(Into::into),
    }
}

fn run_repo_tag_delete(file: &mut VolFile, name: String) -> Result<TagInfo, ErrCtx> {
    let repo = repo_for_file(file)?;
    repo.tag_delete(&name).map_err(Into::into)
}

fn run_repo_switch_branch(
    runtime: &Runtime,
    file: &mut VolFile,
    name: String,
    force: bool,
) -> Result<RepoSwitchOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot switch branches while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    let plan = repo.plan_switch_branch(&name)?;
    prepare_repo_switch_checkout(runtime, file, &repo, &plan, force)?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    let paths = checkout_plan_path_actions(&plan, &previous_files, &previous_artifacts);
    repo.apply_switch_branch_plan(&name, &plan)?;
    checkout_repo_plan(
        runtime,
        file,
        &repo,
        &plan,
        &previous_files,
        &previous_artifacts,
        None,
    )?;
    Ok(RepoSwitchOutcome { branch: name, target: plan.target, paths })
}

fn run_repo_switch_create(
    runtime: &Runtime,
    file: &mut VolFile,
    name: String,
    start_point: Option<String>,
    force: bool,
) -> Result<RepoSwitchCreateOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot switch branches while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    let plan = repo.plan_switch_new_branch(&name, start_point.as_deref())?;
    prepare_repo_switch_checkout(runtime, file, &repo, &plan.checkout, force)?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    let paths = checkout_plan_path_actions(&plan.checkout, &previous_files, &previous_artifacts);
    let branch = repo.apply_switch_new_branch_plan(&plan)?;
    checkout_repo_plan(
        runtime,
        file,
        &repo,
        &plan.checkout,
        &previous_files,
        &previous_artifacts,
        None,
    )?;
    Ok(RepoSwitchCreateOutcome { branch, paths })
}

fn prepare_repo_switch_checkout(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    plan: &CheckoutPlan,
    force: bool,
) -> Result<(), ErrCtx> {
    if repo_has_work_in_progress_for_file(runtime, file, repo)? {
        if force {
            repo.discard_work_in_progress()?;
        } else {
            return pragma_err!("cannot switch branches with staged or unstaged changes");
        }
    }
    if !force {
        ensure_checkout_plan_preserves_untracked_paths(runtime, file, repo, plan)?;
    }
    verify_repo_checkout_plan(runtime, plan, None)
}

fn json_commit_summary(commit: CommitObject) -> JsonCommitSummary {
    let parents = if commit.parents.is_empty() {
        commit.parent.into_iter().collect()
    } else {
        commit.parents
    };
    JsonCommitSummary {
        id: commit.id,
        message: commit.message,
        parents,
    }
}

fn json_commit_path_changes(commit: &CommitObject) -> Vec<crate::json::JsonRepoPathDiff> {
    commit
        .changes
        .iter()
        .map(|change| crate::json::JsonRepoPathDiff {
            path: change.path.clone(),
            change: repo_file_change_label(change.change).to_string(),
            kind: repo_tracked_path_kind_json_label(change.kind).to_string(),
            storage: repo_path_storage_json_label(change.storage).to_string(),
        })
        .collect()
}

fn run_repo_merge(
    runtime: &Runtime,
    file: &mut VolFile,
    rev: &str,
) -> Result<RepoMergeCommandOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot merge while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    if repo_has_work_in_progress_for_file(runtime, file, &repo)? {
        return pragma_err!("cannot merge with staged or unstaged changes");
    }
    clear_row_conflict_resolution_state(&repo)?;
    let plan = repo.plan_merge_revision(rev)?;
    let plan = prepare_repo_merge_plan(runtime, &plan, None)?;
    ensure_checkout_plan_preserves_untracked_paths(runtime, file, &repo, &plan.checkout)?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    let mut outcome = repo.apply_merge_plan(&plan)?;
    checkout_merge_outcome(
        runtime,
        file,
        &repo,
        &outcome,
        Some(&plan.checkout),
        &previous_files,
        &previous_artifacts,
        None,
    )?;
    let row_auto_merge =
        match try_row_auto_merge_current_file_conflict(runtime, file, &repo, &outcome, None) {
            Ok(row_auto_merge) => row_auto_merge,
            Err(err) => {
                tracing::warn!("row-level auto-merge unavailable: {err}");
                None
            }
        };
    if let Some(row_auto_merge) = &row_auto_merge {
        outcome = merge_outcome_with_row_auto_merge(&outcome, &row_auto_merge.key);
    }
    let paths = merge_path_actions(
        &repo,
        &outcome,
        Some(&plan.checkout),
        &previous_files,
        &previous_artifacts,
    )?;
    let branch = repo.current_branch()?;
    Ok(RepoMergeCommandOutcome { outcome, branch, paths, row_auto_merge })
}

fn merge_fast_forward_head(outcome: &MergeOutcome) -> Option<String> {
    match outcome {
        MergeOutcome::FastForward { to, .. } => Some(to.clone()),
        MergeOutcome::AlreadyUpToDate { .. } | MergeOutcome::Merged { .. } => None,
    }
}

fn run_repo_pull(
    runtime: &Runtime,
    file: &mut VolFile,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
) -> Result<RepoPullCommandOutcome, ErrCtx> {
    let repo = repo_for_file(file)?;
    if all {
        return pragma_err!("pull does not support --all; fetch --all first, then pull one branch");
    }
    if !file.is_idle() {
        return pragma_err!("cannot pull while there is an open transaction");
    }
    if repo_has_work_in_progress_for_file(runtime, file, &repo)? {
        return pragma_err!("cannot pull with staged or unstaged changes");
    }
    let local_branch = repo
        .current_branch()?
        .ok_or_else(|| ErrCtx::PragmaErr("cannot pull in detached HEAD".into()))?;
    let (remote, mut plan) = if let Some(refspec) = refspec {
        let remote = repo_default_remote(&repo, remote)?;
        let plan = repo.plan_pull_refspec(&remote, &refspec, &local_branch)?;
        (remote, plan)
    } else {
        let upstream = repo_remote_branch(&repo, remote, branch)?;
        let plan = repo.plan_pull(&upstream.remote, &upstream.branch, &local_branch)?;
        (upstream.remote, plan)
    };
    let checkout_remote = Arc::new(repo.remote_store(&remote)?);
    plan.merge = prepare_repo_merge_plan(runtime, &plan.merge, Some(checkout_remote.clone()))?;
    ensure_checkout_plan_preserves_untracked_paths(runtime, file, &repo, &plan.merge.checkout)?;
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
    clear_row_conflict_resolution_state(&repo)?;
    let mut outcome = repo.apply_pull_plan(&plan)?;
    checkout_merge_outcome(
        runtime,
        file,
        &repo,
        &outcome.merge,
        Some(&plan.merge.checkout),
        &previous_files,
        &previous_artifacts,
        Some(checkout_remote.clone()),
    )?;
    if let Ok(Some(row_auto_merge)) = try_row_auto_merge_current_file_conflict(
        runtime,
        file,
        &repo,
        &outcome.merge,
        Some(checkout_remote),
    ) {
        outcome.merge = merge_outcome_with_row_auto_merge(&outcome.merge, &row_auto_merge.key);
    }
    let paths = merge_path_actions(
        &repo,
        &outcome.merge,
        Some(&plan.merge.checkout),
        &previous_files,
        &previous_artifacts,
    )?;
    let status = repo.status()?;
    let current_head = status.head_target;
    let current_branch = repo.current_branch()?;
    Ok(RepoPullCommandOutcome {
        outcome,
        current_head,
        current_branch,
        paths,
    })
}

fn run_repo_push(
    runtime: &Runtime,
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
    force: bool,
) -> Result<PushCommandOutcome, ErrCtx> {
    if let Some(refspec) = refspec {
        let remote = repo_default_remote(repo, remote)?;
        publish_repo_refspec_snapshots(runtime, repo, &remote, &refspec)?;
        let outcome = repo.push_refspec_with_force(&remote, &refspec, force)?;
        Ok(PushCommandOutcome::Many(outcome))
    } else if all {
        let remote = repo_default_remote(repo, remote)?;
        publish_repo_all_branch_snapshots(runtime, repo, &remote)?;
        let outcome = repo.push_all_with_force(&remote, force)?;
        Ok(PushCommandOutcome::Many(outcome))
    } else {
        let (remote, local_branch, remote_branch) = repo_push_branches(repo, remote, branch)?;
        let remote_head = repo.remote_branch_head_state(&remote, &remote_branch)?;
        let tracking_head = if !force {
            repo.remote_tracking_ref(&remote, &remote_branch)?
        } else {
            None
        };
        publish_repo_branch_snapshots(
            runtime,
            repo,
            &remote,
            &local_branch,
            tracking_head.as_deref().or(remote_head.head.as_deref()),
        )?;
        let outcome = repo.push_branch_with_force_and_remote_head(
            &remote,
            &local_branch,
            &remote_branch,
            force,
            remote_head,
        )?;
        Ok(PushCommandOutcome::One(outcome))
    }
}

fn format_push_command_outcome(outcome: &PushCommandOutcome) -> Result<String, ErrCtx> {
    match outcome {
        PushCommandOutcome::One(outcome) => Ok(format!(
            "{} {}/{} to {} ({} commits)",
            if outcome.forced {
                "Force-pushed"
            } else {
                "Pushed"
            },
            outcome.remote,
            outcome.remote_branch,
            &outcome.head[..12],
            outcome.commits
        )),
        PushCommandOutcome::Many(outcome) => format_push_all_outcome(outcome),
    }
}

fn json_push_command_outcome(
    repo: &Repository,
    outcome: &PushCommandOutcome,
) -> Result<JsonPushCommandOutcome, ErrCtx> {
    let (current_head, current_branch) = repo_head_and_branch(repo)?;
    Ok(JsonPushCommandOutcome {
        operation: "push",
        current_head,
        current_branch,
        remote: outcome.remote(),
        branches: outcome.branches(),
        commits: outcome.commits(),
        forced: outcome.forced(),
    })
}

fn run_repo_checkout(
    runtime: &Runtime,
    file: &mut VolFile,
    spec: RepoCheckoutSpec,
) -> Result<JsonCheckoutOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot checkout while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    match spec {
        RepoCheckoutSpec::Detach { rev, force } => {
            let plan = repo.plan_detach(&rev)?;
            if repo_has_work_in_progress_for_file(runtime, file, &repo)? {
                if force {
                    repo.discard_work_in_progress()?;
                } else {
                    return pragma_err!("cannot checkout with staged or unstaged changes");
                }
            }
            if !force {
                ensure_checkout_plan_preserves_untracked_paths(runtime, file, &repo, &plan)?;
            }
            verify_repo_checkout_plan(runtime, &plan, None)?;
            let previous_files = current_repo_files_for_checkout(&repo)?;
            let previous_artifacts = current_repo_artifacts_for_checkout(&repo)?;
            let id = repo.apply_detach_plan(&rev, &plan)?;
            checkout_repo_plan(
                runtime,
                file,
                &repo,
                &plan,
                &previous_files,
                &previous_artifacts,
                None,
            )?;
            let (current_head, current_branch) = repo_head_and_branch(&repo)?;
            Ok(JsonCheckoutOutcome {
                operation: "checkout",
                current_head: current_head.clone(),
                current_branch: current_branch.clone(),
                head: current_head,
                branch: current_branch,
                target: id,
                path: None,
                paths: Vec::new(),
                path_details: Vec::new(),
            })
        }
        RepoCheckoutSpec::Path { rev, path } => {
            let path = repo_path_arg(&repo, &path)?;
            checkout_repo_path_from_revision(runtime, file, &repo, &rev, &path)
        }
    }
}

fn checkout_repo_path_from_revision(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    rev: &str,
    path: &str,
) -> Result<JsonCheckoutOutcome, ErrCtx> {
    match checkout_repo_key_from_revision(runtime, file, repo, rev, path) {
        Ok((target, path_detail)) => {
            let path = path_detail.path.clone();
            let (current_head, current_branch) = repo_head_and_branch(repo)?;
            Ok(JsonCheckoutOutcome {
                operation: "checkout",
                current_head: current_head.clone(),
                current_branch: current_branch.clone(),
                head: current_head,
                branch: current_branch,
                target,
                path: Some(path),
                paths: Vec::new(),
                path_details: vec![path_detail],
            })
        }
        Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotFoundInRevision { .. })) => {
            let keys = checkout_keys_for_revision_pathspec(repo, rev, path)?;
            if keys.is_empty() {
                return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotFoundInRevision {
                    path: path.to_string(),
                    rev: rev.to_string(),
                }));
            }
            let target = repo.resolve_revision(rev)?;
            let mut checkout_keys = BTreeSet::new();
            for key in &keys {
                if revision_has_repo_key(repo, &target, key)? {
                    checkout_keys.insert(key.clone());
                }
            }
            ensure_checkout_keys_preserve_untracked_paths(runtime, file, repo, &checkout_keys)?;
            let mut checked_out = Vec::with_capacity(keys.len());
            let mut path_details = Vec::with_capacity(keys.len());
            for key in keys {
                if revision_has_repo_key(repo, &target, &key)? {
                    let (_, path_detail) =
                        checkout_repo_key_from_revision(runtime, file, repo, rev, &key)?;
                    checked_out.push(path_detail.path.clone());
                    path_details.push(path_detail);
                } else {
                    let path_detail = current_key_path_detail(repo, &key)?;
                    stage_checkout_deletion_for_key(runtime, file, repo, &key)?;
                    checked_out.push(path_detail.path.clone());
                    path_details.push(path_detail);
                }
            }
            let (current_head, current_branch) = repo_head_and_branch(repo)?;
            Ok(JsonCheckoutOutcome {
                operation: "checkout",
                current_head: current_head.clone(),
                current_branch: current_branch.clone(),
                head: current_head,
                branch: current_branch,
                target,
                path: None,
                paths: checked_out,
                path_details,
            })
        }
        Err(err) => Err(err.into()),
    }
}

fn checkout_repo_key_from_revision(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    rev: &str,
    key: &str,
) -> Result<(String, JsonPathDetail), ErrCtx> {
    let current_key = repo.file_key(&file.tag)?;
    match repo.plan_checkout_file_key_from_revision(rev, key.to_string()) {
        Ok(plan) => {
            ensure_checkout_key_preserves_untracked_path(runtime, file, repo, &plan.path)?;
            hydrate_repo_file_state(runtime, &plan.state, None)?;
            let outcome = repo.apply_checkout_file_plan(&plan)?;
            if outcome.path == current_key {
                checkout_repo_file_state(runtime, file, &outcome.state, None)?;
            } else {
                checkout_repo_file_state_to_key(
                    runtime,
                    repo,
                    &outcome.path,
                    &outcome.state,
                    None,
                )?;
            }
            Ok((
                outcome.target,
                json_path_detail(
                    outcome.path,
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                ),
            ))
        }
        Err(graft::repo::RepoErr::PathNotFoundInRevision { .. }) => {
            let plan = repo.plan_checkout_artifact_key_from_revision(rev, key.to_string())?;
            ensure_checkout_key_preserves_untracked_path(runtime, file, repo, &plan.path)?;
            let outcome = repo.apply_checkout_artifact_plan(&plan)?;
            if outcome.path == current_key {
                let volume = runtime.volume_open(None, None, None)?;
                file.switch_volume(&volume.vid)?;
            }
            repo.materialize_artifact_key(&outcome.path, &outcome.state)?;
            Ok((
                outcome.target,
                json_path_detail(
                    outcome.path,
                    artifact_checkout_path_kind(&outcome.state),
                    artifact_checkout_path_storage(&outcome.state),
                ),
            ))
        }
        Err(err) => Err(err.into()),
    }
}

fn json_path_detail(
    path: String,
    kind: RepoTrackedPathKind,
    storage: RepoPathStorage,
) -> JsonPathDetail {
    JsonPathDetail {
        path,
        kind: repo_tracked_path_kind_json_label(kind),
        storage: repo_path_storage_json_label(storage),
    }
}

fn artifact_checkout_path_kind(state: &CommitArtifactState) -> RepoTrackedPathKind {
    match state {
        CommitArtifactState::File { kind, .. } | CommitArtifactState::LargeFile { kind, .. } => {
            *kind
        }
    }
}

fn artifact_checkout_path_storage(state: &CommitArtifactState) -> RepoPathStorage {
    match state {
        CommitArtifactState::File { .. } => RepoPathStorage::Inline,
        CommitArtifactState::LargeFile { .. } => RepoPathStorage::External,
    }
}

fn current_key_path_detail(repo: &Repository, key: &str) -> Result<JsonPathDetail, ErrCtx> {
    if repo.index_files()?.contains_key(key) {
        return Ok(json_path_detail(
            key.to_string(),
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
        ));
    }

    if let Some(state) = repo.index_artifacts()?.get(key) {
        return Ok(json_path_detail(
            key.to_string(),
            artifact_checkout_path_kind(state),
            artifact_checkout_path_storage(state),
        ));
    }
    match repo.show_revision("HEAD") {
        Ok(commit) => {
            if commit.files.contains_key(key) {
                return Ok(json_path_detail(
                    key.to_string(),
                    RepoTrackedPathKind::SqliteDatabase,
                    RepoPathStorage::SqliteSnapshot,
                ));
            }
            if let Some(state) = commit.artifacts.get(key) {
                return Ok(json_path_detail(
                    key.to_string(),
                    artifact_checkout_path_kind(state),
                    artifact_checkout_path_storage(state),
                ));
            }
        }
        Err(graft::repo::RepoErr::UnbornHead) => {}
        Err(err) => return Err(err.into()),
    }

    Ok(json_path_detail(
        key.to_string(),
        RepoTrackedPathKind::BinaryFile,
        RepoPathStorage::Inline,
    ))
}

fn checkout_keys_for_revision_pathspec(
    repo: &Repository,
    rev: &str,
    filter: &str,
) -> Result<Vec<String>, ErrCtx> {
    let target = repo.resolve_revision(rev)?;
    let commit = repo.read_commit(&target)?;
    let mut keys = BTreeSet::new();
    keys.extend(
        commit
            .files
            .keys()
            .chain(commit.artifacts.keys())
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    keys.extend(
        repo.index_files()?
            .keys()
            .chain(repo.index_artifacts()?.keys())
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    Ok(keys.into_iter().collect())
}

fn revision_has_repo_key(repo: &Repository, target: &str, key: &str) -> Result<bool, ErrCtx> {
    let commit = repo.read_commit(target)?;
    Ok(commit.files.contains_key(key) || commit.artifacts.contains_key(key))
}

fn stage_checkout_deletion_for_key(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    key: &str,
) -> Result<(), ErrCtx> {
    let current_key = repo.file_key(&file.tag)?;
    if key == current_key {
        if head_has_repo_key(repo, key)? {
            repo.stage_file_removal_key(key)?;
        } else if repo.index_has_key(key)? {
            repo.restore_index_key_from_head(key)?;
        } else {
            repo.stage_file_removal_key(key)?;
        }
        let volume = runtime.volume_open(None, None, None)?;
        file.switch_volume(&volume.vid)?;
    } else {
        remove_materialized_repo_file(repo, key)?;
        if head_has_repo_key(repo, key)? {
            repo.stage_file_removal_key(key)?;
        } else if repo.index_has_key(key)? {
            repo.restore_index_key_from_head(key)?;
        } else {
            repo.stage_file_removal_key(key)?;
        }
    }
    Ok(())
}

fn head_has_repo_key(repo: &Repository, key: &str) -> Result<bool, ErrCtx> {
    match repo.show_revision("HEAD") {
        Ok(commit) => Ok(commit.files.contains_key(key) || commit.artifacts.contains_key(key)),
        Err(graft::repo::RepoErr::UnbornHead) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn format_checkout_outcome(outcome: &JsonCheckoutOutcome) -> String {
    match &outcome.path {
        Some(path) => format!(
            "Checked out {} from {}",
            path,
            &outcome.target[..outcome.target.len().min(12)]
        ),
        None if !outcome.paths.is_empty() => {
            let mut output = format!(
                "Checked out {} paths from {}",
                outcome.paths.len(),
                &outcome.target[..outcome.target.len().min(12)]
            );
            for path in &outcome.paths {
                output.push_str("\n  ");
                output.push_str(path);
            }
            output
        }
        None => format!(
            "HEAD detached at {}",
            &outcome.target[..outcome.target.len().min(12)]
        ),
    }
}

fn run_repo_reset(
    runtime: &Runtime,
    file: &mut VolFile,
    rev: &str,
    mode: ResetMode,
) -> Result<RepoResetCommandOutcome, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot reset while there is an open transaction");
    }

    let repo = repo_for_file(file)?;
    let current_state = current_repo_file_state(runtime, file)?;
    let old_head_state = repo.head_file(&file.tag)?;
    let had_staged_changes = repo.has_staged_changes()?;
    let plan = repo.plan_reset(rev, mode)?;
    let plan = if matches!(mode, ResetMode::Hard) {
        let mut plan = plan;
        plan.checkout = prepare_repo_checkout_plan_with_hash_policy(
            runtime,
            &plan.checkout,
            None,
            SnapshotHashPolicy::AllowHydratedMismatch,
        )?;
        plan
    } else {
        plan
    };
    if matches!(mode, ResetMode::Hard) {
        verify_repo_checkout_plan(runtime, &plan.checkout, None)?;
    }
    let previous_files = if matches!(mode, ResetMode::Hard) {
        current_repo_files_for_checkout(&repo)?
    } else {
        BTreeMap::new()
    };
    let previous_artifacts = if matches!(mode, ResetMode::Hard) {
        current_repo_artifacts_for_checkout(&repo)?
    } else {
        BTreeMap::new()
    };
    let reset_paths = if matches!(mode, ResetMode::Hard) {
        checkout_plan_path_actions(&plan.checkout, &previous_files, &previous_artifacts)
    } else {
        Vec::new()
    };
    let outcome = repo.apply_reset_plan(&plan)?;

    match mode {
        ResetMode::Soft => {
            if !had_staged_changes && let Some(old_head_state) = &old_head_state {
                let target_state = repo.head_file(&file.tag)?;
                if target_state.as_ref() != Some(old_head_state) {
                    repo.stage_file_state_path(&file.tag, old_head_state.clone())?;
                }
            }
            if !had_staged_changes
                && old_head_state
                    .as_ref()
                    .is_some_and(|old_head_state| &current_state != old_head_state)
            {
                repo.mark_dirty_path(&file.tag)?;
            }
        }
        ResetMode::Mixed => {
            let target_state = repo.head_file(&file.tag)?;
            if target_state.as_ref() == Some(&current_state) {
                repo.clear_dirty_path(&file.tag)?;
            } else {
                repo.mark_dirty_path(&file.tag)?;
            }
        }
        ResetMode::Hard => {
            checkout_repo_plan(
                runtime,
                file,
                &repo,
                &plan.checkout,
                &previous_files,
                &previous_artifacts,
                None,
            )?;
        }
    }

    let branch = repo.current_branch()?;
    Ok(RepoResetCommandOutcome { outcome, branch, paths: reset_paths })
}

fn checkout_plan_path_actions(
    plan: &CheckoutPlan,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
) -> Vec<JsonPathAction> {
    let mut paths = BTreeMap::new();
    for path in plan.files.keys() {
        paths.insert(
            path.clone(),
            json_path_action(
                path.clone(),
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
                "checked_out",
            ),
        );
    }
    for (path, state) in &plan.artifacts {
        paths.insert(
            path.clone(),
            json_path_action(
                path.clone(),
                artifact_checkout_path_kind(state),
                artifact_checkout_path_storage(state),
                "checked_out",
            ),
        );
    }
    for path in previous_files.keys() {
        if plan.files.contains_key(path) || plan.artifacts.contains_key(path) {
            continue;
        }
        paths.insert(
            path.clone(),
            json_path_action(
                path.clone(),
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
                "removed",
            ),
        );
    }
    for (path, state) in previous_artifacts {
        if plan.files.contains_key(path) || plan.artifacts.contains_key(path) {
            continue;
        }
        paths.insert(
            path.clone(),
            json_path_action(
                path.clone(),
                artifact_checkout_path_kind(state),
                artifact_checkout_path_storage(state),
                "removed",
            ),
        );
    }
    paths.into_values().collect()
}

fn merge_path_actions(
    repo: &Repository,
    outcome: &MergeOutcome,
    fast_forward_plan: Option<&CheckoutPlan>,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
) -> Result<Vec<JsonPathAction>, ErrCtx> {
    let mut paths = BTreeMap::new();
    match outcome {
        MergeOutcome::FastForward { .. } => {
            if let Some(plan) = fast_forward_plan {
                return Ok(checkout_plan_path_actions(
                    plan,
                    previous_files,
                    previous_artifacts,
                ));
            }
        }
        MergeOutcome::Merged { staged, conflicted, .. } => {
            let materialized = conflicted.is_empty();
            let index = repo.read_index()?;
            let stage0_entries = index
                .stage0_entries()
                .map(|entry| (entry.path.clone(), entry.clone()))
                .collect::<BTreeMap<_, _>>();
            for path in staged {
                let Some(entry) = stage0_entries.get(path) else {
                    continue;
                };
                let action = if materialized {
                    "checked_out"
                } else {
                    "staged"
                };
                let path_action = if entry.file.is_some() {
                    json_path_action(
                        path.clone(),
                        RepoTrackedPathKind::SqliteDatabase,
                        RepoPathStorage::SqliteSnapshot,
                        action,
                    )
                } else if let Some(state) = &entry.artifact {
                    json_path_action(
                        path.clone(),
                        artifact_checkout_path_kind(state),
                        artifact_checkout_path_storage(state),
                        action,
                    )
                } else {
                    let (kind, storage) =
                        previous_path_descriptor(path, previous_files, previous_artifacts);
                    json_path_action(
                        path.clone(),
                        kind,
                        storage,
                        if materialized { "removed" } else { "staged" },
                    )
                };
                paths.insert(path.clone(), path_action);
            }
            for path in conflicted {
                paths.insert(
                    path.clone(),
                    json_path_action(
                        path.clone(),
                        conflict_path_kind(repo, path)?,
                        conflict_path_storage(repo, path)?,
                        "conflicted",
                    ),
                );
            }
        }
        MergeOutcome::AlreadyUpToDate { .. } => {}
    }
    Ok(paths.into_values().collect())
}

fn previous_path_descriptor(
    path: &str,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
) -> (RepoTrackedPathKind, RepoPathStorage) {
    if previous_files.contains_key(path) {
        (
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
        )
    } else if let Some(state) = previous_artifacts.get(path) {
        (
            artifact_checkout_path_kind(state),
            artifact_checkout_path_storage(state),
        )
    } else {
        (RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline)
    }
}

fn json_path_action(
    path: String,
    kind: RepoTrackedPathKind,
    storage: RepoPathStorage,
    action: &'static str,
) -> JsonPathAction {
    JsonPathAction {
        path,
        kind: repo_tracked_path_kind_json_label(kind),
        storage: repo_path_storage_json_label(storage),
        action,
    }
}

fn checkout_repo_head(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    if let Some(state) = repo.head_file(&file.tag)? {
        checkout_repo_file_state(runtime, file, &state, remote)?;
    } else {
        let volume = runtime.volume_open(None, None, None)?;
        file.switch_volume(&volume.vid)?;
    }
    Ok(())
}

fn checkout_repo_plan(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    plan: &CheckoutPlan,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let key = repo.file_key(&file.tag)?;
    if let Some(state) = plan.files.get(&key) {
        checkout_repo_file_state(runtime, file, state, remote.clone())?;
    } else {
        let volume = runtime.volume_open(None, None, None)?;
        file.switch_volume(&volume.vid)?;
    }
    for (path, state) in &plan.files {
        if path == &key {
            continue;
        }
        checkout_repo_file_state_to_path(
            runtime,
            repo,
            state,
            &repo.worktree().join(path),
            remote.clone(),
        )?;
    }
    repo.materialize_artifact_checkout(&plan.artifacts, previous_artifacts, &plan.files)?;
    for path in previous_files.keys() {
        if path == &key || plan.files.contains_key(path) || plan.artifacts.contains_key(path) {
            continue;
        }
        remove_materialized_repo_file(repo, path)?;
    }
    Ok(())
}

fn current_repo_files_for_checkout(
    repo: &Repository,
) -> Result<BTreeMap<String, CommitFileState>, ErrCtx> {
    match repo.index_files() {
        Ok(files) => Ok(files),
        Err(graft::repo::RepoErr::UnresolvedConflicts) => Ok(BTreeMap::new()),
        Err(err) => Err(err.into()),
    }
}

fn current_repo_artifacts_for_checkout(
    repo: &Repository,
) -> Result<BTreeMap<String, graft::repo::CommitArtifactState>, ErrCtx> {
    match repo.index_artifacts() {
        Ok(artifacts) => Ok(artifacts),
        Err(graft::repo::RepoErr::UnresolvedConflicts) => Ok(BTreeMap::new()),
        Err(err) => Err(err.into()),
    }
}

fn remove_materialized_repo_file(repo: &Repository, key: &str) -> Result<(), ErrCtx> {
    let path = repo.worktree().join(key);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(ErrCtx::PragmaErr(
                    format!("path `{}` is not a regular file", path.display()).into(),
                ));
            }
            std::fs::remove_file(path)?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

enum RestoredRepoPathState {
    File(CommitFileState),
    Artifact(CommitArtifactState),
}

fn restore_repo_path(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    if spec.all {
        return restore_repo_staged_all(runtime, file, repo, spec);
    }

    let path = spec.path.as_deref().ok_or_else(|| {
        ErrCtx::PragmaErr("restore requires a path unless --staged --all is used".into())
    })?;
    let (key, physical_path) = repo_physical_path_arg(repo, path)?;
    let is_directory = std::fs::symlink_metadata(&physical_path)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false);
    if is_directory {
        return restore_repo_directory(runtime, file, repo, spec, &key);
    }

    match restore_repo_key(runtime, file, repo, spec, &key) {
        Ok(restored) => json_restore_outcome(repo, spec, vec![restored]),
        Err(ErrCtx::PragmaErr(_)) | Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(_))) => {
            let keys = restore_keys_for_pathspec(repo, spec, &key)?;
            if keys.is_empty() {
                restore_repo_key(runtime, file, repo, spec, &key)
                    .and_then(|restored| json_restore_outcome(repo, spec, vec![restored]))
            } else {
                restore_repo_keys(runtime, file, repo, spec, keys)
            }
        }
        Err(err) => Err(err),
    }
}

fn restore_repo_staged_all(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    let status = repo_status_for_file(runtime, file, repo)?;
    let keys = status
        .staged_changes
        .into_iter()
        .filter(|change| spec.kind.is_none_or(|kind| change.kind == kind))
        .map(|change| change.path)
        .collect();
    restore_repo_keys(runtime, file, repo, spec, keys)
}

fn restore_repo_directory(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
    key: &str,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    let keys = restore_keys_for_pathspec(repo, spec, key)?;
    if keys.is_empty() {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
            if key.is_empty() {
                ".".to_string()
            } else {
                key.to_string()
            },
        )));
    }
    restore_repo_keys(runtime, file, repo, spec, keys)
}

fn restore_repo_keys(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
    keys: Vec<String>,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    let mut restored = Vec::with_capacity(keys.len());
    for key in keys {
        restored.push(restore_repo_key(runtime, file, repo, spec, &key)?);
    }
    json_restore_outcome(repo, spec, restored)
}

fn restore_repo_key(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
    key: &str,
) -> Result<JsonPathDetail, ErrCtx> {
    if spec.staged {
        let restored = restore_key_path_detail(repo, spec, key)?;
        if let Some(source) = &spec.source {
            repo.restore_index_key_from_revision(source, key)?;
        } else {
            repo.restore_index_key_from_head(key)?;
        }
        update_worktree_state_after_index_restore_key(runtime, file, repo, key)?;
        return Ok(restored);
    }

    let restored = if let Some(source) = &spec.source {
        let source_commit = repo.show_revision(source)?;
        if let Some(state) = source_commit.files.get(key).cloned() {
            Some(RestoredRepoPathState::File(state))
        } else {
            source_commit
                .artifacts
                .get(key)
                .cloned()
                .map(RestoredRepoPathState::Artifact)
        }
    } else {
        if let Some(state) = repo.index_files()?.get(key).cloned() {
            Some(RestoredRepoPathState::File(state))
        } else {
            repo.index_artifacts()?
                .get(key)
                .cloned()
                .map(RestoredRepoPathState::Artifact)
        }
    };

    if restored.is_none() {
        let can_restore_deletion = if spec.source.is_some() {
            repo.index_files()?.contains_key(key)
                || repo.index_artifacts()?.contains_key(key)
                || repo.index_has_key(key)?
                || head_has_repo_key(repo, key)?
        } else {
            repo.index_has_key(key)?
        };
        if !can_restore_deletion {
            return Err(ErrCtx::PragmaErr(
                format!("path `{key}` is not tracked").into(),
            ));
        }
    }

    let path_detail = restored_repo_path_detail(repo, key, restored.as_ref())?;
    let current_key = repo.file_key(&file.tag)?;
    if key == current_key {
        if let Some(RestoredRepoPathState::File(state)) = &restored {
            checkout_repo_file_state(runtime, file, state, None)?;
        } else if let Some(RestoredRepoPathState::Artifact(state)) = &restored {
            let volume = runtime.volume_open(None, None, None)?;
            file.switch_volume(&volume.vid)?;
            repo.materialize_artifact_key(&key, state)?;
        } else {
            let volume = runtime.volume_open(None, None, None)?;
            file.switch_volume(&volume.vid)?;
        }
    } else if let Some(RestoredRepoPathState::File(state)) = &restored {
        checkout_repo_file_state_to_key(runtime, repo, key, state, None)?;
    } else if let Some(RestoredRepoPathState::Artifact(state)) = &restored {
        repo.materialize_artifact_key(key, state)?;
    } else {
        remove_materialized_repo_file(repo, key)?;
    }

    update_restored_worktree_state_key(runtime, repo, key, restored.as_ref())?;
    Ok(path_detail)
}

fn json_restore_outcome(
    repo: &Repository,
    spec: &RepoRestoreSpec,
    path_details: Vec<JsonPathDetail>,
) -> Result<JsonRestoreOutcome, ErrCtx> {
    let paths = path_details
        .iter()
        .map(|path| path.path.clone())
        .collect::<Vec<_>>();
    let path = match paths.as_slice() {
        [path] => Some(path.clone()),
        _ => None,
    };
    let (current_head, current_branch) = repo_head_and_branch(repo)?;
    Ok(JsonRestoreOutcome {
        operation: "restore",
        current_head,
        current_branch,
        source: spec.source.clone(),
        staged: spec.staged,
        all: spec.all,
        kind: spec.kind.map(repo_tracked_path_kind_json_label),
        path,
        paths: if path_details.len() == 1 {
            Vec::new()
        } else {
            paths
        },
        path_details,
    })
}

fn format_restore_outcome(outcome: &JsonRestoreOutcome) -> String {
    let restored = match &outcome.path {
        Some(path) => path.clone(),
        None => format_repo_path_list(
            outcome.path_details.len(),
            outcome
                .path_details
                .iter()
                .map(|path| path.path.clone())
                .collect(),
        ),
    };
    format!("Restored {restored}")
}

fn restored_repo_path_detail(
    repo: &Repository,
    key: &str,
    restored: Option<&RestoredRepoPathState>,
) -> Result<JsonPathDetail, ErrCtx> {
    match restored {
        Some(RestoredRepoPathState::File(_)) => Ok(json_path_detail(
            key.to_string(),
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
        )),
        Some(RestoredRepoPathState::Artifact(state)) => Ok(json_path_detail(
            key.to_string(),
            artifact_checkout_path_kind(state),
            artifact_checkout_path_storage(state),
        )),
        None => current_key_path_detail(repo, key),
    }
}

fn restore_key_path_detail(
    repo: &Repository,
    spec: &RepoRestoreSpec,
    key: &str,
) -> Result<JsonPathDetail, ErrCtx> {
    if let Some(source) = &spec.source {
        let source_commit = repo.show_revision(source)?;
        if source_commit.files.contains_key(key) {
            return Ok(json_path_detail(
                key.to_string(),
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
            ));
        }
        if let Some(state) = source_commit.artifacts.get(key) {
            return Ok(json_path_detail(
                key.to_string(),
                artifact_checkout_path_kind(state),
                artifact_checkout_path_storage(state),
            ));
        }
        return current_key_path_detail(repo, key);
    }

    if spec.staged {
        match repo.show_revision("HEAD") {
            Ok(head) => {
                if head.files.contains_key(key) {
                    return Ok(json_path_detail(
                        key.to_string(),
                        RepoTrackedPathKind::SqliteDatabase,
                        RepoPathStorage::SqliteSnapshot,
                    ));
                }
                if let Some(state) = head.artifacts.get(key) {
                    return Ok(json_path_detail(
                        key.to_string(),
                        artifact_checkout_path_kind(state),
                        artifact_checkout_path_storage(state),
                    ));
                }
            }
            Err(graft::repo::RepoErr::UnbornHead) => {}
            Err(err) => return Err(err.into()),
        }
    }

    current_key_path_detail(repo, key)
}

fn restore_keys_for_pathspec(
    repo: &Repository,
    spec: &RepoRestoreSpec,
    filter: &str,
) -> Result<Vec<String>, ErrCtx> {
    let mut keys = BTreeSet::new();

    if let Some(source) = &spec.source {
        let source_commit = repo.show_revision(source)?;
        keys.extend(
            source_commit
                .files
                .keys()
                .chain(source_commit.artifacts.keys())
                .filter(|key| repo_key_matches_filter(key, filter))
                .cloned(),
        );
    } else if spec.staged {
        if let Ok(head) = repo.show_revision("HEAD") {
            keys.extend(
                head.files
                    .keys()
                    .chain(head.artifacts.keys())
                    .filter(|key| repo_key_matches_filter(key, filter))
                    .cloned(),
            );
        }
    }

    keys.extend(
        repo.index_files()?
            .keys()
            .chain(repo.index_artifacts()?.keys())
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    keys.extend(
        repo.read_index()?
            .stage0_entries()
            .filter(|entry| repo_key_matches_filter(&entry.path, filter))
            .map(|entry| entry.path.clone()),
    );

    Ok(keys.into_iter().collect())
}

fn format_repo_path_list(count: usize, paths: Vec<String>) -> String {
    match paths.as_slice() {
        [path] => path.clone(),
        _ => {
            let mut output = format!("{count} paths");
            for path in paths {
                output.push_str("\n  ");
                output.push_str(&path);
            }
            output
        }
    }
}

fn export_repo_path(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    spec: &RepoExportSpec,
) -> Result<String, ErrCtx> {
    let path = spec.path.as_deref().unwrap_or_else(|| Path::new(&file.tag));
    let (key, physical_path) = repo_physical_path_arg(repo, path)?;

    if let Some(source) = &spec.source {
        let state = repo
            .file_from_revision(source, &physical_path)?
            .ok_or_else(|| ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(key.clone())))?;
        hydrate_repo_file_state_for(runtime, &state, None, RepoSnapshotPurpose::Export)?;
        write_repo_file_state_to_path(runtime, &state, &spec.output)?;
        return Ok(key);
    }

    let current_key = repo.file_key(&file.tag)?;
    if key != current_key {
        return Err(ErrCtx::PragmaErr(
            format!(
                "exporting worktree path `{key}` requires opening that database path or passing --source"
            )
            .into(),
        ));
    }

    let reader = file.reader()?;
    write_volume_reader_to_path(&reader, &spec.output)?;
    Ok(key)
}

fn update_worktree_state_after_index_restore_key(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    key: &str,
) -> Result<(), ErrCtx> {
    if key != repo.file_key(&file.tag)? {
        repo.clear_dirty_key(key)?;
        return Ok(());
    }

    let worktree_state = current_repo_file_state(runtime, file)?;
    let index_state = repo.index_files()?.get(key).cloned();
    let matches_index = match index_state.as_ref() {
        Some(index_state) => repo_file_state_content_eq(runtime, &worktree_state, index_state)?,
        None => false,
    };
    if matches_index {
        repo.clear_dirty_key(key)?;
    } else {
        repo.mark_dirty_key(key.to_string())?;
    }
    Ok(())
}

fn update_restored_worktree_state_key(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    restored: Option<&RestoredRepoPathState>,
) -> Result<(), ErrCtx> {
    let index_state = repo.index_files()?.get(key).cloned();
    let index_artifact = repo.index_artifacts()?.get(key).cloned();
    let matches_index = match (restored, index_state.as_ref(), index_artifact.as_ref()) {
        (Some(RestoredRepoPathState::File(restored)), Some(index_state), None) => {
            repo_file_state_content_eq(runtime, restored, index_state)?
        }
        (Some(RestoredRepoPathState::Artifact(restored)), None, Some(index_artifact)) => {
            restored == index_artifact
        }
        (None, None, None) => true,
        _ => false,
    };

    if matches_index {
        repo.clear_dirty_key(key)?;
    } else if restored.is_none() {
        repo.mark_deleted_key(key.to_string())?;
    } else {
        repo.mark_dirty_key(key.to_string())?;
    }
    Ok(())
}

fn checkout_repo_file_state(
    runtime: &Runtime,
    file: &mut VolFile,
    state: &CommitFileState,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let snapshot = state.snapshot.to_snapshot();
    let volume = if snapshot.is_empty() {
        runtime.volume_open(None, None, None)?
    } else {
        hydrate_repo_file_state(runtime, state, remote)?;
        runtime.volume_from_snapshot(&snapshot)?
    };
    file.switch_volume(&volume.vid)?;
    Ok(())
}

fn checkout_repo_file_state_to_path(
    runtime: &Runtime,
    repo: &Repository,
    state: &CommitFileState,
    path: &Path,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let key = repo.file_key(path)?;
    let path = repo.worktree().join(key);
    if let Ok(metadata) = std::fs::symlink_metadata(&path)
        && !metadata.file_type().is_file()
    {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular SQLite database file",
                path.display()
            )
            .into(),
        ));
    }

    hydrate_repo_file_state(runtime, state, remote)?;
    write_repo_file_state_to_path(runtime, state, &path)
}

fn checkout_repo_file_state_to_key(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    state: &CommitFileState,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let path = repo.worktree().join(key);
    if let Ok(metadata) = std::fs::symlink_metadata(&path)
        && !metadata.file_type().is_file()
    {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular SQLite database file",
                path.display()
            )
            .into(),
        ));
    }

    hydrate_repo_file_state(runtime, state, remote)?;
    write_repo_file_state_to_path(runtime, state, &path)
}

fn write_empty_sqlite_file_to_path(path: &Path) -> Result<(), ErrCtx> {
    write_sqlite_file_to_path(path, |_| Ok(()))
}

fn write_volume_reader_to_path<R: VolumeRead>(reader: &R, path: &Path) -> Result<(), ErrCtx> {
    write_sqlite_file_to_path(path, |output| {
        for page_idx in reader.page_count().iter() {
            let page = reader.read_page(page_idx)?;
            output.write_all(page.as_ref())?;
        }
        Ok(())
    })
}

fn write_sqlite_file_to_path(
    path: &Path,
    mut write_contents: impl FnMut(&mut File) -> Result<(), ErrCtx>,
) -> Result<(), ErrCtx> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular SQLite database file",
                path.display()
            )
            .into(),
        ));
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let started_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for attempt in 0..100 {
        let tmp = parent.join(format!(
            ".graft-checkout-{}-{started_ms}-{attempt}",
            std::process::id()
        ));
        if tmp.exists() {
            continue;
        }

        let write_result = (|| -> Result<(), ErrCtx> {
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            write_contents(&mut output)?;
            output.flush()?;
            Ok(())
        })();

        match write_result.and_then(|()| {
            std::fs::rename(&tmp, path)?;
            Ok(())
        }) {
            Ok(()) => return Ok(()),
            Err(err) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(err);
            }
        }
    }

    Err(ErrCtx::PragmaErr(
        format!(
            "could not create temporary checkout file for `{}`",
            path.display()
        )
        .into(),
    ))
}

fn write_repo_file_state_to_path(
    runtime: &Runtime,
    state: &CommitFileState,
    path: &Path,
) -> Result<(), ErrCtx> {
    let snapshot = state.snapshot.to_snapshot();
    if snapshot.is_empty() {
        return write_empty_sqlite_file_to_path(path);
    }
    let volume = runtime.volume_from_snapshot(&snapshot)?;
    let reader = runtime.volume_reader(volume.vid)?;
    write_volume_reader_to_path(&reader, path)
}

fn checkout_merge_outcome(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    outcome: &MergeOutcome,
    fast_forward_plan: Option<&CheckoutPlan>,
    previous_files: &BTreeMap<String, CommitFileState>,
    previous_artifacts: &BTreeMap<String, graft::repo::CommitArtifactState>,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    match outcome {
        MergeOutcome::FastForward { .. } => {
            if let Some(plan) = fast_forward_plan {
                checkout_repo_plan(
                    runtime,
                    file,
                    repo,
                    plan,
                    previous_files,
                    previous_artifacts,
                    remote,
                )?;
            } else {
                checkout_repo_head(runtime, file, repo, remote)?;
            }
        }
        MergeOutcome::Merged { staged, conflicted, .. } if conflicted.is_empty() => {
            let key = repo.file_key(&file.tag)?;
            let index = repo.read_index()?;
            for entry in index.stage0_entries() {
                if !staged.iter().any(|path| path == &entry.path) {
                    continue;
                }

                if entry.path == key {
                    if let Some(state) = &entry.file {
                        checkout_repo_file_state(runtime, file, state, remote.clone())?;
                    } else if let Some(state) = &entry.artifact {
                        repo.materialize_artifact_key(&entry.path, state)?;
                    } else {
                        let volume = runtime.volume_open(None, None, None)?;
                        file.switch_volume(&volume.vid)?;
                    }
                } else if let Some(state) = &entry.file {
                    checkout_repo_file_state_to_path(
                        runtime,
                        repo,
                        state,
                        &repo.worktree().join(&entry.path),
                        remote.clone(),
                    )?;
                } else if let Some(state) = &entry.artifact {
                    repo.materialize_artifact_key(&entry.path, state)?;
                } else {
                    remove_materialized_repo_file(repo, &entry.path)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn hydrate_repo_file_state(
    runtime: &Runtime,
    state: &CommitFileState,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    hydrate_repo_file_state_for(runtime, state, remote, RepoSnapshotPurpose::Checkout)
}

fn hydrate_repo_file_state_for(
    runtime: &Runtime,
    state: &CommitFileState,
    remote: Option<Arc<Remote>>,
    purpose: RepoSnapshotPurpose,
) -> Result<(), ErrCtx> {
    RepoSnapshotResolver::strict(runtime, remote, purpose)
        .resolve_file_state(state)
        .map(|_| ())
}

fn prepare_repo_snapshot_for_push(
    runtime: &Runtime,
    snapshot: &RepoSnapshot,
) -> Result<(), ErrCtx> {
    RepoSnapshotResolver::normalizing(
        runtime,
        None,
        RepoSnapshotPurpose::Push,
        SnapshotHashPolicy::AllowHydratedMismatch,
    )
    .resolve_snapshot(snapshot)
    .map(|_| ())
}

fn verify_repo_checkout_plan(
    runtime: &Runtime,
    plan: &CheckoutPlan,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    RepoSnapshotResolver::strict(runtime, remote, RepoSnapshotPurpose::Checkout)
        .resolve_checkout_plan(plan)
        .map(|_| ())
}

fn prepare_repo_checkout_plan(
    runtime: &Runtime,
    plan: &CheckoutPlan,
    remote: Option<Arc<Remote>>,
) -> Result<CheckoutPlan, ErrCtx> {
    let hash_policy = if remote.is_some() {
        SnapshotHashPolicy::AllowHydratedMismatch
    } else {
        SnapshotHashPolicy::Strict
    };
    RepoSnapshotResolver::normalizing(runtime, remote, RepoSnapshotPurpose::Checkout, hash_policy)
        .resolve_checkout_plan(plan)
}

fn prepare_repo_checkout_plan_with_hash_policy(
    runtime: &Runtime,
    plan: &CheckoutPlan,
    remote: Option<Arc<Remote>>,
    hash_policy: SnapshotHashPolicy,
) -> Result<CheckoutPlan, ErrCtx> {
    RepoSnapshotResolver::normalizing(runtime, remote, RepoSnapshotPurpose::Reset, hash_policy)
        .resolve_checkout_plan(plan)
}

fn prepare_repo_merge_plan(
    runtime: &Runtime,
    plan: &MergePlan,
    remote: Option<Arc<Remote>>,
) -> Result<MergePlan, ErrCtx> {
    let hash_policy = if remote.is_some() {
        SnapshotHashPolicy::AllowHydratedMismatch
    } else {
        SnapshotHashPolicy::Strict
    };
    RepoSnapshotResolver::normalizing(runtime, remote, RepoSnapshotPurpose::Merge, hash_policy)
        .resolve_merge_plan(plan)
}

impl<'a> RepoSnapshotResolver<'a> {
    fn strict(
        runtime: &'a Runtime,
        remote: Option<Arc<Remote>>,
        purpose: RepoSnapshotPurpose,
    ) -> Self {
        let remote_mode = if remote.is_some() {
            RepoSnapshotRemoteMode::Remote
        } else {
            RepoSnapshotRemoteMode::LocalOnly
        };
        Self {
            runtime,
            remote,
            policy: RepoSnapshotResolvePolicy {
                purpose,
                remote_mode,
                hash_policy: SnapshotHashPolicy::Strict,
                normalize: false,
            },
        }
    }

    fn normalizing(
        runtime: &'a Runtime,
        remote: Option<Arc<Remote>>,
        purpose: RepoSnapshotPurpose,
        hash_policy: SnapshotHashPolicy,
    ) -> Self {
        let remote_mode = if remote.is_some() {
            RepoSnapshotRemoteMode::Remote
        } else {
            RepoSnapshotRemoteMode::LocalOnly
        };
        Self {
            runtime,
            remote,
            policy: RepoSnapshotResolvePolicy {
                purpose,
                remote_mode,
                hash_policy,
                normalize: hash_policy != SnapshotHashPolicy::Strict,
            },
        }
    }

    fn local_then_remote(
        runtime: &'a Runtime,
        remote: Option<Arc<Remote>>,
        purpose: RepoSnapshotPurpose,
        hash_policy: SnapshotHashPolicy,
    ) -> Self {
        Self {
            runtime,
            remote,
            policy: RepoSnapshotResolvePolicy {
                purpose,
                remote_mode: RepoSnapshotRemoteMode::LocalThenRemote,
                hash_policy,
                normalize: false,
            },
        }
    }

    fn resolve_file_state(&self, state: &CommitFileState) -> Result<CommitFileState, ErrCtx> {
        let resolved = self.resolve_snapshot(&state.snapshot)?;
        Ok(CommitFileState {
            volume: state.volume.clone(),
            snapshot: resolved.snapshot,
        })
    }

    fn resolve_checkout_plan(&self, plan: &CheckoutPlan) -> Result<CheckoutPlan, ErrCtx> {
        let mut plan = plan.clone();
        for state in plan.files.values_mut() {
            *state = self.resolve_file_state(state)?;
        }
        Ok(plan)
    }

    fn resolve_merge_plan(&self, plan: &MergePlan) -> Result<MergePlan, ErrCtx> {
        let mut plan = plan.clone();
        if matches!(plan.outcome, MergeOutcome::FastForward { .. }) {
            plan.checkout = self.resolve_checkout_plan(&plan.checkout)?;
        }
        if let Some(index) = &mut plan.index {
            for entry in &mut index.entries {
                if let Some(state) = entry.file.clone() {
                    entry.file = Some(self.resolve_file_state(&state)?);
                }
            }
        }
        Ok(plan)
    }

    fn resolve_snapshot(&self, snapshot: &RepoSnapshot) -> Result<ResolvedRepoSnapshot, ErrCtx> {
        if self.policy.remote_mode == RepoSnapshotRemoteMode::Remote {
            return self.resolve_snapshot_once(
                snapshot,
                RepoSnapshotResolveSource::Remote,
                self.remote.clone(),
            );
        }

        match self.resolve_snapshot_once(snapshot, RepoSnapshotResolveSource::Local, None) {
            Ok(resolved) => Ok(resolved),
            Err(local_err)
                if self.policy.remote_mode == RepoSnapshotRemoteMode::LocalThenRemote =>
            {
                let Some(remote) = self.remote.clone() else {
                    return Err(local_err);
                };
                self.resolve_snapshot_once(snapshot, RepoSnapshotResolveSource::Remote, Some(remote))
                    .map_err(|remote_err| {
                        ErrCtx::PragmaErr(
                            format!(
                                "local snapshot hydrate failed: {local_err}; remote snapshot hydrate failed: {remote_err}"
                            )
                            .into(),
                        )
                    })
            }
            Err(err) => Err(err),
        }
    }

    fn resolve_snapshot_once(
        &self,
        snapshot: &RepoSnapshot,
        source: RepoSnapshotResolveSource,
        remote: Option<Arc<Remote>>,
    ) -> Result<ResolvedRepoSnapshot, ErrCtx> {
        let runtime_snapshot = snapshot.to_snapshot();
        if !runtime_snapshot.is_empty() {
            match source {
                RepoSnapshotResolveSource::Local => {
                    for range in &snapshot.ranges {
                        self.runtime.fetch_log(range.log.clone(), Some(range.end))?;
                    }
                    self.runtime.snapshot_hydrate(runtime_snapshot.clone())?;
                }
                RepoSnapshotResolveSource::Remote => {
                    let Some(remote) = remote else {
                        return Err(ErrCtx::PragmaErr(
                            "snapshot resolver remote source requires a remote".into(),
                        ));
                    };
                    self.runtime
                        .snapshot_hydrate_from(runtime_snapshot.clone(), remote)?;
                }
            }
        }

        let hash_mismatches =
            verify_repo_snapshot_commit_hashes(self.runtime, snapshot, self.policy.hash_policy)?;
        let resolved_snapshot =
            if self.policy.normalize && self.policy.hash_policy != SnapshotHashPolicy::Strict {
                repo_snapshot_with_commit_hashes(self.runtime, &runtime_snapshot)?
            } else {
                snapshot.clone()
            };
        let resolved = ResolvedRepoSnapshot {
            snapshot: resolved_snapshot,
            runtime_snapshot,
            source,
            hash_mismatches,
        };
        resolved.trace_if_needed(self.policy.purpose);
        Ok(resolved)
    }
}

impl ResolvedRepoSnapshot {
    fn trace_if_needed(&self, purpose: RepoSnapshotPurpose) {
        if self.hash_mismatches > 0 {
            tracing::warn!(
                mismatches = self.hash_mismatches,
                source = ?self.source,
                purpose = ?purpose,
                "snapshot storage commit hashes mismatched; using hydrated storage commit hashes"
            );
        } else if matches!(self.source, RepoSnapshotResolveSource::Remote) {
            tracing::debug!(
                source = ?self.source,
                purpose = ?purpose,
                ranges = self.snapshot.ranges.len(),
                runtime_ranges = self.runtime_snapshot.iter().count(),
                pages = self.snapshot.page_count.to_u32(),
                "resolved repository snapshot from remote"
            );
        }
    }
}

fn verify_repo_snapshot_commit_hashes(
    runtime: &Runtime,
    snapshot: &RepoSnapshot,
    hash_policy: SnapshotHashPolicy,
) -> Result<usize, ErrCtx> {
    let mut mismatches = 0_usize;
    for range in &snapshot.ranges {
        let mut expected_commits = range.commits.iter();
        for lsn in (range.start..=range.end).iter() {
            let Some(expected) = expected_commits.next() else {
                return Err(ErrCtx::PragmaErr(
                    format!(
                        "snapshot references missing storage commit hash for {:?}/{}",
                        range.log, lsn
                    )
                    .into(),
                ));
            };
            if expected.lsn != lsn {
                if expected.lsn > lsn {
                    return Err(ErrCtx::PragmaErr(
                        format!(
                            "snapshot references missing storage commit hash for {:?}/{}",
                            range.log, lsn
                        )
                        .into(),
                    ));
                }
                return Err(ErrCtx::PragmaErr(
                    format!(
                        "snapshot storage commit hash out of order for {:?}: expected LSN {}, got {}",
                        range.log, lsn, expected.lsn
                    )
                    .into(),
                ));
            }
            let Some(actual) = repo_storage_commit_hash(runtime, &range.log, lsn)? else {
                return Err(ErrCtx::PragmaErr(
                    format!(
                        "snapshot references missing storage commit {:?}/{}",
                        range.log, lsn
                    )
                    .into(),
                ));
            };
            if actual != expected.commit_hash {
                match hash_policy {
                    SnapshotHashPolicy::Strict => {
                        return Err(ErrCtx::PragmaErr(
                            format!(
                                "snapshot storage commit hash mismatch for {:?}/{}: expected {}, got {}",
                                range.log, lsn, expected.commit_hash, actual
                            )
                            .into(),
                        ));
                    }
                    SnapshotHashPolicy::AllowHydratedMismatch => {
                        mismatches += 1;
                    }
                }
            }
        }
        if let Some(extra) = expected_commits.next() {
            return Err(ErrCtx::PragmaErr(
                format!(
                    "snapshot references extra storage commit hash for {:?}/{} outside {}..={}",
                    range.log, extra.lsn, range.start, range.end
                )
                .into(),
            ));
        }
    }
    Ok(mismatches)
}

fn repo_storage_commit_hash(
    runtime: &Runtime,
    log: &LogId,
    lsn: LSN,
) -> Result<Option<graft::core::commit_hash::CommitHash>, ErrCtx> {
    let Some(commit) = runtime.get_commit(log, lsn)? else {
        return Ok(None);
    };
    if let Some(commit_hash) = commit.commit_hash().cloned() {
        return Ok(Some(commit_hash));
    }
    runtime.commit_hash(log, lsn).map_err(ErrCtx::from)
}

fn publish_repo_branch_snapshots(
    runtime: &Runtime,
    repo: &Repository,
    remote: &str,
    branch: &str,
    stop_at: Option<&str>,
) -> Result<(), ErrCtx> {
    let remote_store = Arc::new(repo.remote_store(remote)?);
    let mut stop_commits = BTreeSet::<String>::new();
    if let Some(stop_at) = stop_at {
        stop_commits.insert(stop_at.to_string());
    } else {
        stop_commits.extend(repo_remote_reachable_commits_known_locally(repo, remote)?);
    }
    let mut stack = vec![
        repo.branch_target(branch)?
            .ok_or(ErrCtx::Repo(graft::repo::RepoErr::UnbornHead))?,
    ];
    let mut seen = std::collections::BTreeSet::<String>::new();
    let mut snapshots = Vec::new();

    while let Some(next) = stack.pop() {
        if !seen.insert(next.clone()) {
            continue;
        }
        if stop_commits.contains(&next) {
            continue;
        }
        let commit = repo.read_commit(&next)?;
        let parent_files = repo_commit_parent_file_states(repo, &commit)?;
        for (path, state) in &commit.files {
            let snapshot = repo_file_delta_snapshot(
                state,
                parent_files.get(path).map(Vec::as_slice).unwrap_or(&[]),
            );
            let runtime_snapshot = snapshot.to_snapshot();
            if runtime_snapshot.is_empty() {
                continue;
            }
            prepare_repo_snapshot_for_push(runtime, &snapshot)?;
            snapshots.push(runtime_snapshot);
        }

        if commit.parents.is_empty() {
            if let Some(parent) = commit.parent {
                stack.push(parent);
            }
        } else {
            stack.extend(commit.parents);
        }
    }

    runtime.snapshots_push_to(snapshots, remote_store)?;

    Ok(())
}

fn repo_remote_reachable_commits_known_locally(
    repo: &Repository,
    remote: &str,
) -> Result<BTreeSet<String>, ErrCtx> {
    let roots = repo
        .remote_branch_refs(remote)?
        .into_iter()
        .map(|branch| branch.head)
        .collect::<Vec<_>>();
    Ok(repo_reachable_commits_known_locally(repo, roots))
}

fn repo_reachable_commits_known_locally(
    repo: &Repository,
    roots: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
    let mut reachable = BTreeSet::new();
    let mut stack = roots.into_iter().collect::<Vec<_>>();
    while let Some(next) = stack.pop() {
        if !reachable.insert(next.clone()) {
            continue;
        }
        let Ok(commit) = repo.read_commit(&next) else {
            continue;
        };
        stack.extend(repo_commit_parent_ids(&commit));
    }
    reachable
}

fn repo_commit_parent_file_states(
    repo: &Repository,
    commit: &CommitObject,
) -> Result<BTreeMap<String, Vec<CommitFileState>>, ErrCtx> {
    let mut files = BTreeMap::<String, Vec<CommitFileState>>::new();
    for parent in repo_commit_parent_ids(commit) {
        for (path, state) in repo.read_commit(&parent)?.files {
            files.entry(path).or_default().push(state);
        }
    }
    Ok(files)
}

fn repo_commit_parent_ids(commit: &CommitObject) -> Vec<String> {
    if commit.parents.is_empty() {
        commit.parent.iter().cloned().collect()
    } else {
        commit.parents.clone()
    }
}

fn repo_file_delta_snapshot(
    state: &CommitFileState,
    parent_states: &[CommitFileState],
) -> RepoSnapshot {
    let coverage = repo_file_parent_coverage(state, parent_states);
    let mut ranges = Vec::new();
    for range in &state.snapshot.ranges {
        let intervals = coverage.get(&range.log).map(Vec::as_slice).unwrap_or(&[]);
        append_uncovered_repo_log_ranges(&mut ranges, range, intervals);
    }

    RepoSnapshot {
        page_count: state.snapshot.page_count,
        ranges,
    }
}

fn repo_file_parent_coverage(
    state: &CommitFileState,
    parent_states: &[CommitFileState],
) -> BTreeMap<LogId, Vec<(LSN, LSN)>> {
    let mut coverage = BTreeMap::<LogId, Vec<(LSN, LSN)>>::new();
    for parent_state in parent_states {
        if parent_state.volume != state.volume {
            continue;
        }
        for range in &parent_state.snapshot.ranges {
            coverage
                .entry(range.log.clone())
                .or_default()
                .push((range.start, range.end));
        }
    }

    for intervals in coverage.values_mut() {
        intervals.sort_by_key(|(start, _)| *start);
        let mut merged = Vec::<(LSN, LSN)>::new();
        for (start, end) in intervals.drain(..) {
            if let Some((_, current_end)) = merged.last_mut() {
                if current_end.checked_next().is_none_or(|next| start <= next) {
                    if end > *current_end {
                        *current_end = end;
                    }
                    continue;
                }
            }
            merged.push((start, end));
        }
        *intervals = merged;
    }

    coverage
}

fn append_uncovered_repo_log_ranges(
    ranges: &mut Vec<RepoLogRange>,
    range: &RepoLogRange,
    covered_intervals: &[(LSN, LSN)],
) {
    let mut cursor = Some(range.start);

    for (covered_start, covered_end) in covered_intervals {
        let Some(start) = cursor else {
            break;
        };
        if *covered_end < start {
            continue;
        }
        if *covered_start > range.end {
            break;
        }
        if *covered_start > start {
            let end = covered_start
                .checked_prev()
                .unwrap_or(range.end)
                .min(range.end);
            if start <= end {
                push_repo_log_range(ranges, range, start, end);
            }
        }
        if *covered_end >= range.end {
            cursor = None;
            break;
        }
        cursor = covered_end.checked_next();
    }

    if let Some(start) = cursor {
        if start <= range.end {
            push_repo_log_range(ranges, range, start, range.end);
        }
    }
}

fn push_repo_log_range(
    ranges: &mut Vec<RepoLogRange>,
    source: &RepoLogRange,
    start: LSN,
    end: LSN,
) {
    ranges.push(RepoLogRange {
        log: source.log.clone(),
        start,
        end,
        commits: source
            .commits
            .iter()
            .filter(|commit| commit.lsn >= start && commit.lsn <= end)
            .cloned()
            .collect(),
    });
}

fn publish_repo_all_branch_snapshots(
    runtime: &Runtime,
    repo: &Repository,
    remote: &str,
) -> Result<(), ErrCtx> {
    for branch in repo.branches()? {
        if branch.target.is_some() {
            let stop_at = repo.remote_branch_head(remote, &branch.name)?;
            publish_repo_branch_snapshots(runtime, repo, remote, &branch.name, stop_at.as_deref())?;
        }
    }
    Ok(())
}

fn publish_repo_refspec_snapshots(
    runtime: &Runtime,
    repo: &Repository,
    remote: &str,
    refspec: &str,
) -> Result<(), ErrCtx> {
    for branch in repo.push_refspec_branches(refspec)? {
        let stop_at = repo.remote_branch_head(remote, &branch.remote_branch)?;
        publish_repo_branch_snapshots(
            runtime,
            repo,
            remote,
            &branch.local_branch,
            stop_at.as_deref(),
        )?;
    }
    Ok(())
}

fn parse_remote_add(arg: &str) -> Result<(String, RemoteConfig), PragmaErr> {
    let (name, uri) = arg
        .split_once(char::is_whitespace)
        .ok_or_else(|| pragma_fail("argument must be in the form: `name remote-uri`"))?;
    Ok((
        name.trim().to_string(),
        parse_remote_config_uri(uri.trim())?,
    ))
}

fn parse_remote_rename(arg: &str) -> Result<(String, String), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [old, new] => Ok(((*old).to_string(), (*new).to_string())),
        _ => Err(pragma_fail("argument must be in the form: `old new`")),
    }
}

fn parse_repo_clone_arg(arg: &str) -> Result<RepoCloneSpec, PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let (uri, branch) = match parts.as_slice() {
        [uri] => (*uri, None),
        [uri, branch] => (*uri, Some(*branch)),
        ["--branch" | "-b", branch, uri] => (*uri, Some(*branch)),
        _ => {
            return Err(pragma_fail(
                "argument must be in the form: `remote-uri [branch]` or `--branch branch remote-uri`",
            ));
        }
    };
    if branch.is_some_and(str::is_empty) {
        return Err(pragma_fail("branch name must not be empty"));
    }
    Ok(RepoCloneSpec {
        config: parse_remote_config_uri(uri)?,
        branch: branch.map(str::to_string),
    })
}

fn parse_remote_config_uri(uri: &str) -> Result<RemoteConfig, PragmaErr> {
    if uri.is_empty() {
        return Err(pragma_fail("remote URI must not be empty"));
    }

    Ok(if uri == "memory" {
        RemoteConfig::Memory
    } else if let Some(root) = uri.strip_prefix("fs://") {
        RemoteConfig::Fs { root: root.to_string() }
    } else if let Some(rest) = uri
        .strip_prefix("s3://")
        .or_else(|| uri.strip_prefix("s3_compatible://"))
    {
        let (path, endpoint) = parse_s3_remote_uri_query(rest)?;
        let (bucket, prefix) = path
            .split_once('/')
            .map_or((path, None), |(bucket, prefix)| (bucket, Some(prefix)));
        if bucket.is_empty() {
            return Err(pragma_fail("S3 remote URI must include a bucket"));
        }
        RemoteConfig::S3Compatible {
            bucket: bucket.to_string(),
            prefix: prefix
                .filter(|prefix| !prefix.is_empty())
                .map(ToString::to_string),
            endpoint,
        }
    } else if let Some(rest) = uri.strip_prefix("graft+https://") {
        let (path, token_env) = parse_http_remote_uri_query(rest)?;
        RemoteConfig::Http {
            url: format!("https://{path}"),
            token_env,
        }
    } else if let Some(rest) = uri.strip_prefix("graft+http://") {
        let (path, token_env) = parse_http_remote_uri_query(rest)?;
        RemoteConfig::Http { url: format!("http://{path}"), token_env }
    } else {
        return Err(pragma_fail(
            "remote URI must start with memory, fs://, s3://, s3_compatible://, graft+https://, or graft+http://",
        ));
    })
}

fn parse_s3_remote_uri_query(uri: &str) -> Result<(&str, Option<String>), PragmaErr> {
    let (path, query) = uri
        .split_once('?')
        .map_or((uri, ""), |(path, query)| (path, query));
    if query.is_empty() {
        return Ok((path, None));
    }

    let mut endpoint = None;
    for part in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part
            .split_once('=')
            .map_or((part, ""), |(key, value)| (key, value));
        match key {
            "endpoint" => {
                if value.is_empty() {
                    return Err(pragma_fail("S3 remote endpoint must not be empty"));
                }
                if endpoint.replace(value.to_string()).is_some() {
                    return Err(pragma_fail("S3 remote endpoint specified more than once"));
                }
            }
            _ => {
                return Err(pragma_fail(format!(
                    "unsupported S3 remote URI query parameter `{key}`"
                )));
            }
        }
    }

    Ok((path, endpoint))
}

fn parse_http_remote_uri_query(uri: &str) -> Result<(&str, Option<String>), PragmaErr> {
    let (path, query) = uri
        .split_once('?')
        .map_or((uri, ""), |(path, query)| (path, query));
    if path.is_empty() {
        return Err(pragma_fail(
            "Graft HTTP remote URI must include a host and path",
        ));
    }
    if query.is_empty() {
        return Ok((path, None));
    }

    let mut token_env = None;
    for part in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part
            .split_once('=')
            .map_or((part, ""), |(key, value)| (key, value));
        match key {
            "token_env" => {
                if value.is_empty() {
                    return Err(pragma_fail("Graft HTTP remote token_env must not be empty"));
                }
                if token_env.replace(value.to_string()).is_some() {
                    return Err(pragma_fail(
                        "Graft HTTP remote token_env specified more than once",
                    ));
                }
            }
            _ => {
                return Err(pragma_fail(format!(
                    "unsupported Graft HTTP remote URI query parameter `{key}`"
                )));
            }
        }
    }

    Ok((path, token_env))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteBranchArg {
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
    force: bool,
}

fn parse_remote_branch_arg(arg: Option<&str>) -> Result<RemoteBranchArg, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RemoteBranchArg {
            remote: None,
            branch: None,
            refspec: None,
            all: false,
            force: false,
        });
    };
    let mut all = false;
    let mut force = false;
    let mut positional = Vec::new();
    for part in arg.split_whitespace() {
        match part {
            "--all" => all = true,
            "--force" | "-f" => force = true,
            part => positional.push(part),
        }
    }

    if all {
        return match positional.as_slice() {
            [] => Ok(RemoteBranchArg {
                remote: None,
                branch: None,
                refspec: None,
                all,
                force,
            }),
            [remote] => Ok(RemoteBranchArg {
                remote: Some((*remote).to_string()),
                branch: None,
                refspec: None,
                all,
                force,
            }),
            _ => Err(pragma_fail(
                "argument must be in the form: `[--force] [remote] [branch]` or `[--force] --all [remote]`",
            )),
        };
    }

    match positional.as_slice() {
        [] => Ok(RemoteBranchArg {
            remote: None,
            branch: None,
            refspec: None,
            all,
            force,
        }),
        [remote_or_refspec] if looks_like_refspec(remote_or_refspec) => Ok(RemoteBranchArg {
            remote: None,
            branch: None,
            refspec: Some((*remote_or_refspec).to_string()),
            all,
            force,
        }),
        [remote] => Ok(RemoteBranchArg {
            remote: Some((*remote).to_string()),
            branch: None,
            refspec: None,
            all,
            force,
        }),
        [remote, branch_or_refspec] if looks_like_refspec(branch_or_refspec) => {
            Ok(RemoteBranchArg {
                remote: Some((*remote).to_string()),
                branch: None,
                refspec: Some((*branch_or_refspec).to_string()),
                all,
                force,
            })
        }
        [remote, branch] => Ok(RemoteBranchArg {
            remote: Some((*remote).to_string()),
            branch: Some((*branch).to_string()),
            refspec: None,
            all,
            force,
        }),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] [remote] [branch]` or `[--force] --all [remote]`",
        )),
    }
}

fn looks_like_refspec(value: &str) -> bool {
    let value = value.strip_prefix('+').unwrap_or(value);
    value.contains(':') || value.contains('*') || value.starts_with("refs/")
}

fn parse_repo_diff_arg(arg: Option<&str>) -> Result<RepoDiffSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RepoDiffSpec {
            mode: DiffMode::Default,
            kind: None,
            target: RepoDiffTarget::Worktree { path: None },
        });
    };
    let raw_parts = split_pragma_words(arg)?;
    let mut mode = DiffMode::Default;
    let mut kind = None;
    let mut parts = Vec::new();
    let mut in_path = false;
    let mut index = 0;
    while index < raw_parts.len() {
        let part = &raw_parts[index];
        if !in_path && part == "--" {
            in_path = true;
            parts.push(part.as_str());
            index += 1;
        } else if !in_path && part == "--rows" {
            if mode == DiffMode::Rows {
                return Err(pragma_fail("`--rows` may only be specified once"));
            }
            mode = DiffMode::Rows;
            index += 1;
        } else if !in_path && part == "--kind" {
            if kind.is_some() {
                return Err(pragma_fail("diff accepts --kind only once"));
            }
            let Some(value) = raw_parts.get(index + 1) else {
                return Err(pragma_fail("diff --kind requires a value"));
            };
            kind = Some(parse_repo_tracked_path_kind_arg(value)?);
            index += 2;
        } else {
            parts.push(part.as_str());
            index += 1;
        }
    }
    let target = match parts.as_slice() {
        [] => RepoDiffTarget::Worktree { path: None },
        ["--", path @ ..] if !path.is_empty() => {
            RepoDiffTarget::Worktree { path: Some(path.join(" ")) }
        }
        ["--staged"] | ["--cached"] => RepoDiffTarget::Staged { path: None },
        ["--staged", "--", path @ ..] | ["--cached", "--", path @ ..] if !path.is_empty() => {
            RepoDiffTarget::Staged { path: Some(path.join(" ")) }
        }
        [rev] => RepoDiffTarget::RevisionToWorktree { rev: (*rev).to_string(), path: None },
        [rev, "--", path @ ..] if !path.is_empty() => RepoDiffTarget::RevisionToWorktree {
            rev: (*rev).to_string(),
            path: Some(path.join(" ")),
        },
        [from, to] => RepoDiffTarget::Revisions {
            from: (*from).to_string(),
            to: (*to).to_string(),
            path: None,
        },
        [from, to, "--", path @ ..] if !path.is_empty() => RepoDiffTarget::Revisions {
            from: (*from).to_string(),
            to: (*to).to_string(),
            path: Some(path.join(" ")),
        },
        _ => {
            return Err(pragma_fail(
                "argument must be in the form: `[--rows] [--staged] [rev] [rev] [-- path]`",
            ));
        }
    };
    Ok(RepoDiffSpec { mode, kind, target })
}

fn parse_volume_diff_arg(arg: &str) -> Result<(LSN, LSN, DiffMode), PragmaErr> {
    let parts: Vec<&str> = arg.split(',').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(pragma_fail(
            "argument must be in the form: `from_lsn,to_lsn[,mode]`",
        ));
    }
    let mode = if parts.len() == 3 {
        match parts[2] {
            "rows" => DiffMode::Rows,
            _ => return Err(pragma_fail("mode must be 'rows' or omitted")),
        }
    } else {
        DiffMode::Default
    };
    Ok((parse_or_fail(parts[0])?, parse_or_fail(parts[1])?, mode))
}

fn parse_debug_diff_lsn_arg(arg: &str) -> Result<(LogRef, LogRef), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [from, to] => Ok((parse_or_fail(from)?, parse_or_fail(to)?)),
        _ => Err(pragma_fail(
            "argument must be in the form: `from_log:from_lsn to_log:to_lsn`",
        )),
    }
}

fn parse_repo_add_arg(arg: Option<&str>) -> Result<RepoAddSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RepoAddSpec {
            path: None,
            force: false,
            all: false,
            kind: None,
        });
    };
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(RepoAddSpec {
            path: None,
            force: false,
            all: false,
            kind: None,
        });
    }

    if arg.split_whitespace().any(|part| part == "--kind") {
        let parts = split_pragma_words(arg)?;
        let mut all = false;
        let mut kind = None;
        let mut index = 0;
        while index < parts.len() {
            match parts[index].as_str() {
                "--all" | "-A" => {
                    if all {
                        return Err(pragma_fail("add accepts --all only once"));
                    }
                    all = true;
                    index += 1;
                }
                "--kind" => {
                    if kind.is_some() {
                        return Err(pragma_fail("add accepts --kind only once"));
                    }
                    let Some(value) = parts.get(index + 1) else {
                        return Err(pragma_fail("add --kind requires a value"));
                    };
                    kind = Some(parse_repo_tracked_path_kind_arg(value)?);
                    index += 2;
                }
                value => {
                    return Err(pragma_fail(format!(
                        "unknown add argument `{value}`; `--kind` may only be used with `--all`"
                    )));
                }
            }
        }
        if !all {
            return Err(pragma_fail("add --kind requires --all"));
        }
        return Ok(RepoAddSpec {
            path: None,
            force: false,
            all: true,
            kind,
        });
    }

    if arg == "--all" || arg == "-A" {
        return Ok(RepoAddSpec {
            path: None,
            force: false,
            all: true,
            kind: None,
        });
    }

    for flag in ["--force", "-f"] {
        if arg == flag {
            return Ok(RepoAddSpec {
                path: None,
                force: true,
                all: false,
                kind: None,
            });
        }
        if let Some(path) = arg.strip_prefix(&format!("{flag} -- ")) {
            return Ok(RepoAddSpec {
                path: Some(PathBuf::from(path)),
                force: true,
                all: false,
                kind: None,
            });
        }
        if let Some(path) = arg.strip_prefix(&format!("{flag} ")) {
            if path == "--all" || path == "-A" {
                return Err(pragma_fail(
                    "argument must be in the form: `[--all|-A]` or `[--force] [path]`",
                ));
            }
            return Ok(RepoAddSpec {
                path: Some(PathBuf::from(path)),
                force: true,
                all: false,
                kind: None,
            });
        }
    }

    if arg.starts_with('-') {
        return Err(pragma_fail(
            "argument must be in the form: `[--all|-A]` or `[--force] [path]`",
        ));
    }

    Ok(RepoAddSpec {
        path: Some(PathBuf::from(arg)),
        force: false,
        all: false,
        kind: None,
    })
}

fn parse_repo_remove_arg(arg: Option<&str>) -> Result<RepoRemoveSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RepoRemoveSpec { path: None, cached: false });
    };
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(RepoRemoveSpec { path: None, cached: false });
    }

    let mut cached = false;
    let mut path = Vec::new();
    let mut index = 0;
    let mut in_path = false;
    while index < parts.len() {
        if in_path {
            path.push(parts[index].clone());
            index += 1;
            continue;
        }
        match parts[index].as_str() {
            "--" => {
                in_path = true;
                index += 1;
            }
            "--cached" => {
                if cached {
                    return Err(pragma_fail("rm accepts --cached only once"));
                }
                cached = true;
                index += 1;
            }
            value if value.starts_with('-') && path.is_empty() => {
                return Err(pragma_fail(format!("unknown rm argument `{value}`")));
            }
            _ => {
                path.extend(parts[index..].iter().cloned());
                break;
            }
        }
    }

    Ok(RepoRemoveSpec {
        path: (!path.is_empty()).then(|| PathBuf::from(path.join(" "))),
        cached,
    })
}

fn parse_repo_audit_arg(arg: Option<&str>) -> Result<RepoAuditSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(RepoAuditSpec { repair: false, remote: None });
    };
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(RepoAuditSpec { repair: false, remote: None });
    }

    let mut repair = false;
    let mut remote = None;
    let mut index = 0;
    while index < parts.len() {
        match parts[index].as_str() {
            "--repair" => {
                if repair {
                    return Err(pragma_fail("audit accepts --repair only once"));
                }
                repair = true;
                index += 1;
            }
            value if value.starts_with('-') => {
                return Err(pragma_fail(format!("unknown audit argument `{value}`")));
            }
            value => {
                if remote.is_some() {
                    return Err(pragma_fail("audit --repair accepts at most one remote"));
                }
                remote = Some(value.to_string());
                index += 1;
            }
        }
    }

    if remote.is_some() && !repair {
        return Err(pragma_fail("audit remote requires --repair"));
    }

    Ok(RepoAuditSpec { repair, remote })
}

fn parse_lfs_fetch_arg(arg: Option<&str>) -> Result<LargeFileFetchSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(LargeFileFetchSpec { remote: None, rev: None });
    };
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(LargeFileFetchSpec { remote: None, rev: None });
    }

    let mut remote = None;
    let mut rev = None;
    let mut index = 0;
    while index < parts.len() {
        match parts[index].as_str() {
            "--remote" => {
                if remote.is_some() {
                    return Err(pragma_fail("payload fetch accepts --remote only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("payload fetch --remote requires a remote name"));
                };
                if value.starts_with('-') {
                    return Err(pragma_fail("payload fetch --remote requires a remote name"));
                }
                remote = Some(value.clone());
                index += 2;
            }
            value if value.starts_with('-') => {
                return Err(pragma_fail(format!(
                    "unknown payload fetch argument `{value}`"
                )));
            }
            value => {
                if rev.is_some() {
                    return Err(pragma_fail("payload fetch accepts at most one revision"));
                }
                rev = Some(value.to_string());
                index += 1;
            }
        }
    }

    Ok(LargeFileFetchSpec { remote, rev })
}

fn parse_lfs_status_arg(arg: Option<&str>) -> Result<LargeFileStatusSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(LargeFileStatusSpec { rev: None });
    };
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(LargeFileStatusSpec { rev: None });
    }
    if parts.len() > 1 {
        return Err(pragma_fail("payload status accepts at most one revision"));
    }
    let rev = &parts[0];
    if rev.starts_with('-') {
        return Err(pragma_fail(format!(
            "unknown payload status argument `{rev}`"
        )));
    }
    Ok(LargeFileStatusSpec { rev: Some(rev.to_string()) })
}

fn parse_lfs_prune_arg(arg: Option<&str>) -> Result<LargeFilePruneSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(LargeFilePruneSpec { dry_run: true });
    };
    let parts = split_pragma_words(arg.trim())?;
    if parts.is_empty() {
        return Ok(LargeFilePruneSpec { dry_run: true });
    }

    let mut dry_run = None;
    for part in parts {
        match part.as_str() {
            "--dry-run" => {
                if dry_run.replace(true).is_some() {
                    return Err(pragma_fail("payload prune accepts only one mode flag"));
                }
            }
            "--force" => {
                if dry_run.replace(false).is_some() {
                    return Err(pragma_fail("payload prune accepts only one mode flag"));
                }
            }
            value => {
                return Err(pragma_fail(format!(
                    "unknown payload prune argument `{value}`; expected `--dry-run` or `--force`"
                )));
            }
        }
    }

    Ok(LargeFilePruneSpec { dry_run: dry_run.unwrap_or(true) })
}

fn parse_status_arg(arg: Option<&str>) -> Result<StatusSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(StatusSpec { kind: None });
    };
    let parts = split_pragma_words(arg)?;
    let mut kind = None;
    let mut index = 0;
    while index < parts.len() {
        match parts[index].as_str() {
            "--kind" => {
                if kind.is_some() {
                    return Err(pragma_fail("status accepts --kind only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("status --kind requires a value"));
                };
                kind = Some(parse_repo_tracked_path_kind_arg(value)?);
                index += 2;
            }
            value => {
                return Err(pragma_fail(format!(
                    "unknown status argument `{value}`; expected `--kind <kind>`"
                )));
            }
        }
    }
    Ok(StatusSpec { kind })
}

fn parse_ls_files_arg(arg: Option<&str>) -> Result<LsFilesSpec, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(LsFilesSpec {
            stage: false,
            details: false,
            others: false,
            kind: None,
        });
    };
    let parts = split_pragma_words(arg)?;
    let mut stage = false;
    let mut details = false;
    let mut others = false;
    let mut kind = None;
    let mut index = 0;
    while index < parts.len() {
        match parts[index].as_str() {
            "--stage" | "-s" => {
                if stage {
                    return Err(pragma_fail("ls-files accepts --stage only once"));
                }
                stage = true;
                index += 1;
            }
            "--details" => {
                if details {
                    return Err(pragma_fail("ls-files accepts --details only once"));
                }
                details = true;
                index += 1;
            }
            "--others" => {
                if others {
                    return Err(pragma_fail("ls-files accepts --others only once"));
                }
                others = true;
                index += 1;
            }
            "--kind" => {
                if kind.is_some() {
                    return Err(pragma_fail("ls-files accepts --kind only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("ls-files --kind requires a value"));
                };
                kind = Some(parse_repo_tracked_path_kind_arg(value)?);
                index += 2;
            }
            value => {
                return Err(pragma_fail(format!(
                    "unknown ls-files argument `{value}`; expected `--stage`, `--details`, `--others`, or `--kind <kind>`"
                )));
            }
        }
    }
    if stage && details {
        return Err(pragma_fail(
            "ls-files --details cannot be used with --stage",
        ));
    }
    if others && stage {
        return Err(pragma_fail("ls-files --others cannot be used with --stage"));
    }
    if others && details {
        return Err(pragma_fail(
            "ls-files --others cannot be used with --details",
        ));
    }
    Ok(LsFilesSpec { stage, details, others, kind })
}

fn parse_repo_tracked_path_kind_arg(value: &str) -> Result<RepoTrackedPathKind, PragmaErr> {
    match value {
        "sqlite" | "sqlite_database" | "sqlite-database" | "database" | "db" => {
            Ok(RepoTrackedPathKind::SqliteDatabase)
        }
        "text" | "text_file" | "text-file" => Ok(RepoTrackedPathKind::TextFile),
        "binary" | "binary_file" | "binary-file" => Ok(RepoTrackedPathKind::BinaryFile),
        _ => Err(pragma_fail(
            "--kind must be one of sqlite_database, text_file, or binary_file",
        )),
    }
}

fn parse_repo_config_set_arg(arg: &str) -> Result<(String, String), PragmaErr> {
    let arg = arg.trim();
    if let Some((key, value)) = arg.split_once(" --") {
        return config_set_parts(key, value);
    }

    let mut parts = arg.splitn(2, char::is_whitespace);
    let key = parts.next().unwrap_or_default();
    let value = parts.next().unwrap_or_default();
    config_set_parts(key, value)
}

fn config_set_parts(key: &str, value: &str) -> Result<(String, String), PragmaErr> {
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() || value.is_empty() {
        return Err(pragma_fail("argument must be in the form: `key -- value`"));
    }
    Ok((key.to_string(), value.to_string()))
}

fn parse_repo_checkout_arg(arg: &str) -> Result<RepoCheckoutSpec, PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [rev] => Ok(RepoCheckoutSpec::Detach { rev: (*rev).to_string(), force: false }),
        ["--force" | "-f", rev] => {
            Ok(RepoCheckoutSpec::Detach { rev: (*rev).to_string(), force: true })
        }
        [rev, "--", path @ ..] if !path.is_empty() => Ok(RepoCheckoutSpec::Path {
            rev: (*rev).to_string(),
            path: path.join(" "),
        }),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] rev [-- path]`",
        )),
    }
}

fn parse_repo_restore_arg(arg: &str) -> Result<RepoRestoreSpec, PragmaErr> {
    let parts = split_pragma_words(arg)?;
    let mut source = None;
    let mut staged = false;
    let mut all = false;
    let mut kind = None;
    let mut path = Vec::new();
    let mut index = 0;
    let mut in_path = false;

    while index < parts.len() {
        if in_path {
            path.push(parts[index].clone());
            index += 1;
            continue;
        }

        match parts[index].as_str() {
            "--" => {
                in_path = true;
                index += 1;
            }
            "--staged" | "--cached" => {
                if staged {
                    return Err(pragma_fail("restore accepts --staged only once"));
                }
                staged = true;
                index += 1;
            }
            "--source" | "-s" => {
                if source.is_some() {
                    return Err(pragma_fail("restore accepts --source only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("restore --source requires a revision"));
                };
                source = Some(value.clone());
                index += 2;
            }
            "--all" | "-A" => {
                if all {
                    return Err(pragma_fail("restore accepts --all only once"));
                }
                all = true;
                index += 1;
            }
            "--kind" => {
                if kind.is_some() {
                    return Err(pragma_fail("restore accepts --kind only once"));
                }
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("restore --kind requires a value"));
                };
                kind = Some(parse_repo_tracked_path_kind_arg(value)?);
                index += 2;
            }
            value if value.starts_with('-') && path.is_empty() => {
                return Err(pragma_fail(format!("unknown restore argument `{value}`")));
            }
            _ => {
                path.extend(parts[index..].iter().cloned());
                break;
            }
        }
    }

    if all {
        if !staged {
            return Err(pragma_fail("restore --all requires --staged"));
        }
        if !path.is_empty() {
            return Err(pragma_fail("restore --all does not accept a path"));
        }
    } else if kind.is_some() {
        return Err(pragma_fail("restore --kind requires --all"));
    } else if path.is_empty() {
        return Err(pragma_fail(
            "argument must be in the form: `[--staged] [--source rev] path` or `--staged --all [--kind kind]`",
        ));
    }

    Ok(RepoRestoreSpec {
        source,
        staged,
        all,
        kind,
        path: (!path.is_empty()).then(|| PathBuf::from(path.join(" "))),
    })
}

fn split_pragma_words(arg: &str) -> Result<Vec<String>, PragmaErr> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut in_word = false;

    for ch in arg.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            in_word = true;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            in_word = true;
            continue;
        }

        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            in_word = true;
            continue;
        }

        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                in_word = true;
            }
            ch if ch.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            ch => {
                current.push(ch);
                in_word = true;
            }
        }
    }

    if escaped {
        current.push('\\');
    }
    if quote.is_some() {
        return Err(pragma_fail("unterminated quoted argument"));
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

fn parse_json_log_arg(arg: Option<&str>) -> Result<JsonLogMode, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(JsonLogMode::LegacyArray);
    };
    let words = split_pragma_words(arg)?;
    match words.as_slice() {
        [] => Ok(JsonLogMode::LegacyArray),
        [flag] if flag == "--with-status" => Ok(JsonLogMode::WithStatus),
        _ => Err(pragma_fail(
            "argument must be empty or in the form: `--with-status`",
        )),
    }
}

fn parse_json_config_list_arg(arg: Option<&str>) -> Result<JsonConfigListMode, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(JsonConfigListMode::LegacyArray);
    };
    let words = split_pragma_words(arg)?;
    match words.as_slice() {
        [] => Ok(JsonConfigListMode::LegacyArray),
        [flag] if flag == "--with-status" => Ok(JsonConfigListMode::WithStatus),
        _ => Err(pragma_fail(
            "argument must be empty or in the form: `--with-status`",
        )),
    }
}

fn parse_json_tags_arg(arg: Option<&str>) -> Result<JsonTagsMode, PragmaErr> {
    let Some(arg) = arg else {
        return Ok(JsonTagsMode::LegacyArray);
    };
    let words = split_pragma_words(arg)?;
    match words.as_slice() {
        [] => Ok(JsonTagsMode::LegacyArray),
        [flag] if flag == "--with-status" => Ok(JsonTagsMode::WithStatus),
        _ => Err(pragma_fail(
            "argument must be empty or in the form: `--with-status`",
        )),
    }
}

fn parse_json_fetch_async_arg(
    arg: Option<&str>,
) -> Result<(RemoteBranchArg, JsonFetchAsyncMode), PragmaErr> {
    let Some(arg) = arg else {
        return Ok((parse_remote_branch_arg(None)?, JsonFetchAsyncMode::LegacyId));
    };

    let mut mode = JsonFetchAsyncMode::LegacyId;
    let mut remote_words = Vec::new();
    for word in split_pragma_words(arg)? {
        if word == "--with-status" {
            if mode == JsonFetchAsyncMode::WithStatus {
                return Err(pragma_fail("argument contains duplicate `--with-status`"));
            }
            mode = JsonFetchAsyncMode::WithStatus;
        } else {
            remote_words.push(word);
        }
    }

    let remote_arg = if remote_words.is_empty() {
        None
    } else {
        Some(remote_words.join(" "))
    };
    Ok((parse_remote_branch_arg(remote_arg.as_deref())?, mode))
}

fn parse_repo_export_arg(arg: &str) -> Result<RepoExportSpec, PragmaErr> {
    let mut source = None;
    let mut output = None;
    let mut path = Vec::new();
    let mut after_path_separator = false;
    let mut parts = split_pragma_words(arg)?.into_iter().peekable();

    while let Some(part) = parts.next() {
        match part.as_str() {
            "--source" | "-s" if !after_path_separator => {
                if source.is_some() {
                    return Err(pragma_fail("export accepts only one source revision"));
                }
                let Some(value) = parts.next() else {
                    return Err(pragma_fail("export --source requires a revision"));
                };
                source = Some(value);
            }
            "--output" | "-o" if !after_path_separator => {
                if output.is_some() {
                    return Err(pragma_fail("export accepts only one output path"));
                }
                let Some(value) = parts.next() else {
                    return Err(pragma_fail("export --output requires a path"));
                };
                output = Some(PathBuf::from(value));
            }
            "--" if !after_path_separator => {
                after_path_separator = true;
            }
            value if value.starts_with('-') && !after_path_separator => {
                return Err(pragma_fail(format!("unknown export option `{value}`")));
            }
            _ => {
                path.push(part);
                if after_path_separator {
                    path.extend(parts);
                    break;
                }
            }
        }
    }

    let Some(output) = output else {
        return Err(pragma_fail(
            "argument must be in the form: `[--source rev] --output output.db [-- path]`",
        ));
    };

    Ok(RepoExportSpec {
        source,
        path: (!path.is_empty()).then(|| PathBuf::from(path.join(" "))),
        output,
    })
}

fn parse_repo_resolve_arg(arg: &str) -> Result<RepoResolveSpec, PragmaErr> {
    let mut side = None;
    let mut row = None;
    let mut path = Vec::new();
    let parts: Vec<&str> = arg.split_whitespace().collect();
    let mut index = 0;

    while index < parts.len() {
        match parts[index] {
            "--ours" => {
                if side.replace(ResolveSide::Ours).is_some() {
                    return Err(pragma_fail("resolve accepts only one side"));
                }
                index += 1;
            }
            "--theirs" => {
                if side.replace(ResolveSide::Theirs).is_some() {
                    return Err(pragma_fail("resolve accepts only one side"));
                }
                index += 1;
            }
            "--manual" => {
                if side.replace(ResolveSide::Manual).is_some() {
                    return Err(pragma_fail("resolve accepts only one side"));
                }
                index += 1;
            }
            "--row" => {
                if row.is_some() {
                    return Err(pragma_fail("resolve accepts only one row selector"));
                }
                let Some(table) = parts.get(index + 1) else {
                    return Err(pragma_fail("resolve --row requires a table name"));
                };
                let Some(rowid) = parts.get(index + 2) else {
                    return Err(pragma_fail("resolve --row requires a rowid"));
                };
                let rowid = rowid
                    .parse::<i64>()
                    .map_err(|_| pragma_fail("resolve --row rowid must be an integer"))?;
                row = Some(RepoResolveRowSpec { table: (*table).to_string(), rowid });
                index += 3;
            }
            "--path" => {
                let Some(value) = parts.get(index + 1) else {
                    return Err(pragma_fail("resolve --path requires a path"));
                };
                path.push(*value);
                index += 2;
            }
            value => {
                path.push(value);
                index += 1;
            }
        }
    }

    let Some(side) = side else {
        return Err(pragma_fail(
            "argument must include `--ours`, `--theirs`, or `--manual`",
        ));
    };

    Ok(RepoResolveSpec {
        side,
        path: (!path.is_empty()).then(|| PathBuf::from(path.join(" "))),
        row,
    })
}

fn parse_branch_delete_arg(arg: &str) -> Result<(String, bool), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), false)),
        ["--force", name] | ["-D", name] | ["-f", name] => Ok(((*name).to_string(), true)),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] name`",
        )),
    }
}

fn parse_branch_list_mode(arg: Option<&str>) -> Result<BranchListMode, PragmaErr> {
    match arg.map(str::trim).filter(|arg| !arg.is_empty()) {
        None => Ok(BranchListMode::Local),
        Some("-r" | "--remote" | "--remotes") => Ok(BranchListMode::Remote),
        Some("-a" | "--all") => Ok(BranchListMode::All),
        Some(_) => Err(pragma_fail(
            "argument must be one of: `--remote`, `-r`, `--all`, `-a`",
        )),
    }
}

fn parse_branch_rename_arg(arg: &str) -> Result<(Option<String>, String, bool), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [new] => Ok((None, (*new).to_string(), false)),
        ["--force" | "-M" | "-f", new] => Ok((None, (*new).to_string(), true)),
        ["--force" | "-M" | "-f", old, new] => {
            Ok((Some((*old).to_string()), (*new).to_string(), true))
        }
        [old, new] => Ok((Some((*old).to_string()), (*new).to_string(), false)),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] [old] new`",
        )),
    }
}

fn parse_branch_upstream_arg(arg: &str) -> Result<(Option<String>, String, String), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [upstream] => {
            let (remote, branch) = parse_remote_branch_ref(upstream)?;
            Ok((None, remote, branch))
        }
        [branch, upstream] => {
            let (remote, remote_branch) = parse_remote_branch_ref(upstream)?;
            Ok((Some((*branch).to_string()), remote, remote_branch))
        }
        _ => Err(pragma_fail(
            "argument must be in the form: `[branch] remote/branch`",
        )),
    }
}

fn parse_remote_branch_ref(value: &str) -> Result<(String, String), PragmaErr> {
    let Some((remote, branch)) = value.split_once('/') else {
        return Err(pragma_fail("upstream must be in the form: `remote/branch`"));
    };
    if remote.is_empty() || branch.is_empty() {
        return Err(pragma_fail("upstream must be in the form: `remote/branch`"));
    }
    Ok((remote.to_string(), branch.to_string()))
}

fn parse_branch_create_arg(arg: &str) -> Result<(String, Option<String>), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), None)),
        [name, start_point] => Ok(((*name).to_string(), Some((*start_point).to_string()))),
        _ => Err(pragma_fail(
            "argument must be in the form: `name [start-point]`",
        )),
    }
}

fn parse_switch_branch_arg(arg: &str) -> Result<(String, bool), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), false)),
        ["--force" | "-f", name] => Ok(((*name).to_string(), true)),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] name`",
        )),
    }
}

fn parse_switch_create_arg(arg: &str) -> Result<(String, Option<String>, bool), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), None, false)),
        ["--force" | "-f", name] => Ok(((*name).to_string(), None, true)),
        ["--force" | "-f", name, start_point] => {
            Ok(((*name).to_string(), Some((*start_point).to_string()), true))
        }
        [name, start_point] => Ok(((*name).to_string(), Some((*start_point).to_string()), false)),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--force] name [start-point]`",
        )),
    }
}

fn parse_tag_create_arg(arg: &str) -> Result<(String, Option<String>, Option<String>), PragmaErr> {
    let arg = arg.trim();
    if let Some(rest) = arg
        .strip_prefix("--annotated ")
        .or_else(|| arg.strip_prefix("-a "))
    {
        let Some((spec, message)) = rest.split_once(" -- ") else {
            return Err(pragma_fail(
                "annotated tag argument must be in the form: `--annotated name [rev] -- message`",
            ));
        };
        let message = message.trim();
        if message.is_empty() {
            return Err(pragma_fail("annotated tag message cannot be empty"));
        }
        let parts: Vec<&str> = spec.split_whitespace().collect();
        return match parts.as_slice() {
            [name] => Ok(((*name).to_string(), None, Some(message.to_string()))),
            [name, target] => Ok((
                (*name).to_string(),
                Some((*target).to_string()),
                Some(message.to_string()),
            )),
            _ => Err(pragma_fail(
                "annotated tag argument must be in the form: `--annotated name [rev] -- message`",
            )),
        };
    }

    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [name] => Ok(((*name).to_string(), None, None)),
        [name, target] => Ok(((*name).to_string(), Some((*target).to_string()), None)),
        _ => Err(pragma_fail("argument must be in the form: `name [rev]`")),
    }
}

fn parse_repo_reset_arg(arg: &str) -> Result<(ResetMode, String), PragmaErr> {
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [rev] => Ok((ResetMode::Mixed, (*rev).to_string())),
        ["--soft", rev] => Ok((ResetMode::Soft, (*rev).to_string())),
        ["--mixed", rev] => Ok((ResetMode::Mixed, (*rev).to_string())),
        ["--hard", rev] => Ok((ResetMode::Hard, (*rev).to_string())),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--soft|--mixed|--hard] rev`",
        )),
    }
}

fn repo_diff_path(repo: &Repository, path: Option<&str>) -> Result<Option<String>, ErrCtx> {
    let Some(path) = path else {
        return Ok(None);
    };
    Ok(Some(repo_path_arg(repo, path)?))
}

fn repo_path_arg(repo: &Repository, path: &str) -> Result<String, ErrCtx> {
    let path = path.trim();
    if path.is_empty() {
        return Ok(String::new());
    }
    let path_obj = Path::new(path);
    if path_obj.is_absolute() {
        return Ok(normalize_repo_path_filter(&repo.file_key(path_obj)?));
    }
    Ok(normalize_repo_path_filter(path))
}

fn normalize_repo_path_filter(path: &str) -> String {
    let path = path.trim().trim_start_matches("./").replace('\\', "/");
    let path = path.trim_end_matches('/');
    if path == "." {
        String::new()
    } else {
        path.to_string()
    }
}

fn repo_physical_path_arg(repo: &Repository, path: &Path) -> Result<(String, PathBuf), ErrCtx> {
    let physical_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.worktree().join(path)
    };
    let key = repo.file_key(&physical_path)?;
    Ok((key.clone(), repo.worktree().join(key)))
}

fn repo_input_path(repo: &Repository, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.worktree().join(path)
    }
}

fn run_repo_add(
    runtime: &Runtime,
    file: &mut VolFile,
    spec: &RepoAddSpec,
) -> Result<Vec<graft::repo::index::IndexEntry>, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot add while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    if spec.all {
        stage_repo_add_all(runtime, file, &repo, spec.kind)
    } else if let Some(path) = spec.path.as_deref() {
        stage_repo_add_path(runtime, file, &repo, path, spec.force)
    } else {
        let state = current_repo_file_state(runtime, file)?;
        Ok(vec![repo.stage_file_state_path(&file.tag, state)?])
    }
}

fn json_staged_entry_paths(
    repo: &Repository,
    entries: &[graft::repo::index::IndexEntry],
) -> Result<Vec<crate::json::JsonRepoPathDiff>, ErrCtx> {
    let head = repo_head_commit(repo)?;
    entries
        .iter()
        .map(|entry| {
            let (kind, storage, change) =
                staged_entry_kind_storage_and_change(head.as_ref(), entry);
            Ok(crate::json::JsonRepoPathDiff {
                path: entry.path.clone(),
                change: repo_file_change_label(change).to_string(),
                kind: repo_tracked_path_kind_json_label(kind).to_string(),
                storage: repo_path_storage_json_label(storage).to_string(),
            })
        })
        .collect()
}

fn filter_tracked_paths_by_kind(
    paths: Vec<RepoTrackedPath>,
    kind: Option<RepoTrackedPathKind>,
) -> Vec<RepoTrackedPath> {
    match kind {
        Some(kind) => paths.into_iter().filter(|path| path.kind == kind).collect(),
        None => paths,
    }
}

fn filter_tracked_path_details_by_kind(
    paths: Vec<RepoTrackedPathDetail>,
    kind: Option<RepoTrackedPathKind>,
) -> Vec<RepoTrackedPathDetail> {
    match kind {
        Some(kind) => paths.into_iter().filter(|path| path.kind == kind).collect(),
        None => paths,
    }
}

fn filter_tracked_path_entries_by_kind(
    paths: Vec<RepoTrackedPathEntry>,
    kind: Option<RepoTrackedPathKind>,
) -> Vec<RepoTrackedPathEntry> {
    match kind {
        Some(kind) => paths.into_iter().filter(|path| path.kind == kind).collect(),
        None => paths,
    }
}

fn filter_repo_status_by_kind(
    mut status: RepoStatus,
    kind: Option<RepoTrackedPathKind>,
) -> RepoStatus {
    let Some(kind) = kind else {
        return status;
    };
    status.unstaged_changes.retain(|change| change.kind == kind);
    status.staged_changes.retain(|change| change.kind == kind);
    status
        .conflicted_changes
        .retain(|change| change.kind == kind);
    status.unstaged = status
        .unstaged_changes
        .iter()
        .map(|change| change.path.clone())
        .collect();
    status.staged = status
        .staged_changes
        .iter()
        .map(|change| change.path.clone())
        .collect();
    status.conflicted = status
        .conflicted_changes
        .iter()
        .map(|change| change.path.clone())
        .collect();
    status.refresh_summary_flags();
    status
}

fn repo_head_commit(repo: &Repository) -> Result<Option<CommitObject>, ErrCtx> {
    match repo.show_revision("HEAD") {
        Ok(commit) => Ok(Some(commit)),
        Err(graft::repo::RepoErr::UnbornHead) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn repo_head_and_branch(repo: &Repository) -> Result<(Option<String>, Option<String>), ErrCtx> {
    let status = repo.status()?;
    let branch = repo.current_branch()?;
    Ok((status.head_target, branch))
}

fn staged_entry_kind_storage_and_change(
    head: Option<&CommitObject>,
    entry: &graft::repo::index::IndexEntry,
) -> (RepoTrackedPathKind, RepoPathStorage, RepoFileChange) {
    if entry.file.is_some() {
        let change = if head.is_some_and(|commit| commit.files.contains_key(&entry.path)) {
            RepoFileChange::Modified
        } else {
            RepoFileChange::Added
        };
        return (
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
            change,
        );
    }

    if let Some(artifact) = &entry.artifact {
        let change = if head.is_some_and(|commit| commit.artifacts.contains_key(&entry.path)) {
            RepoFileChange::Modified
        } else {
            RepoFileChange::Added
        };
        return (
            artifact_checkout_path_kind(artifact),
            artifact_checkout_path_storage(artifact),
            change,
        );
    }

    if head.is_some_and(|commit| commit.files.contains_key(&entry.path)) {
        return (
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
            RepoFileChange::Deleted,
        );
    }

    if let Some(artifact) = head.and_then(|commit| commit.artifacts.get(&entry.path)) {
        return (
            artifact_checkout_path_kind(artifact),
            artifact_checkout_path_storage(artifact),
            RepoFileChange::Deleted,
        );
    }

    (
        RepoTrackedPathKind::BinaryFile,
        RepoPathStorage::Inline,
        RepoFileChange::Deleted,
    )
}

fn stage_repo_add_path(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    path: &Path,
    force: bool,
) -> Result<Vec<graft::repo::index::IndexEntry>, ErrCtx> {
    let physical_path = repo_input_path(repo, path);
    let metadata = std::fs::metadata(&physical_path)?;
    let current_key = repo.file_key(&file.tag)?;

    if metadata.is_dir() {
        let directory = std::fs::canonicalize(&physical_path)?;
        if !directory.starts_with(repo.worktree()) {
            return Err(ErrCtx::Repo(graft::repo::RepoErr::PathOutsideWorktree {
                path: directory,
                worktree: repo.worktree().to_path_buf(),
            }));
        }
        if !force && repo.is_ignored_worktree_path(&directory)? {
            return ignored_add_path_error(repo, &directory);
        }

        let mut paths = BTreeSet::new();
        collect_repo_add_directory_files(repo, &directory, force, &mut paths)?;
        let mut entries = Vec::with_capacity(paths.len());
        for key in paths {
            let physical_path = repo.worktree().join(&key);
            entries.push(stage_repo_add_file(
                runtime,
                file,
                repo,
                &current_key,
                &key,
                &physical_path,
            )?);
        }
        return Ok(entries);
    }

    if !metadata.is_file() {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular file or directory",
                physical_path.display()
            )
            .into(),
        ));
    }

    let (key, physical_path) = repo_physical_path_arg(repo, path)?;
    if !force && repo.is_ignored_worktree_path(&physical_path)? {
        return ignored_add_path_error(repo, &physical_path);
    }
    Ok(vec![stage_repo_add_file(
        runtime,
        file,
        repo,
        &current_key,
        &key,
        &physical_path,
    )?])
}

fn stage_repo_add_all(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    kind: Option<RepoTrackedPathKind>,
) -> Result<Vec<graft::repo::index::IndexEntry>, ErrCtx> {
    let status = repo_status_for_file(runtime, file, repo)?;
    let current_key = repo.file_key(&file.tag)?;
    let mut entries = Vec::with_capacity(status.unstaged_changes.len());

    for change in status.unstaged_changes {
        if kind.is_some_and(|kind| change.kind != kind) {
            continue;
        }
        match change.change {
            RepoWorktreeChangeKind::Modified | RepoWorktreeChangeKind::Untracked => {
                let physical_path = repo.worktree().join(&change.path);
                entries.push(stage_repo_add_file(
                    runtime,
                    file,
                    repo,
                    &current_key,
                    &change.path,
                    &physical_path,
                )?);
            }
            RepoWorktreeChangeKind::Deleted => {
                let entry = repo.stage_file_removal_key(&change.path)?;
                if change.path == current_key {
                    let volume = runtime.volume_open(None, None, None)?;
                    file.switch_volume(&volume.vid)?;
                }
                entries.push(entry);
            }
        }
    }

    Ok(entries)
}

fn stage_repo_add_file(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    current_key: &str,
    key: &str,
    physical_path: &Path,
) -> Result<graft::repo::index::IndexEntry, ErrCtx> {
    if key == current_key {
        let state = current_repo_file_state(runtime, file)?;
        repo.stage_file_state_path(&file.tag, state)
            .map_err(Into::into)
    } else if is_sqlite_database_path(physical_path)? {
        stage_physical_sqlite_file(runtime, repo, key, physical_path)
    } else {
        repo.stage_artifact_path(physical_path).map_err(Into::into)
    }
}

fn collect_repo_add_directory_files(
    repo: &Repository,
    dir: &Path,
    force: bool,
    out: &mut BTreeSet<String>,
) -> Result<(), ErrCtx> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if entry.file_name() == graft::repo::GRAFT_DIR {
                continue;
            }
            if !force && repo.is_ignored_worktree_path(&path)? {
                continue;
            }
            collect_repo_add_directory_files(repo, &path, force, out)?;
        } else if file_type.is_file()
            && (force || !repo.is_ignored_worktree_path(&path)?)
            && !is_sqlite_sidecar_path(&path)
        {
            out.insert(repo.file_key(&path)?);
        }
    }
    Ok(())
}

fn ignored_add_path_error<T>(repo: &Repository, path: &Path) -> Result<T, ErrCtx> {
    let key = repo.file_key(path)?;
    Err(ErrCtx::PragmaErr(
        format!("path `{key}` is ignored; use `--force` to add it").into(),
    ))
}

fn format_added_entries(entries: &[graft::repo::index::IndexEntry]) -> String {
    match entries {
        [entry] => format!("Added {}", entry.path),
        entries => {
            let mut output = format!("Added {} paths", entries.len());
            for entry in entries {
                output.push_str("\n  ");
                output.push_str(&entry.path);
            }
            output
        }
    }
}

fn run_repo_remove(
    runtime: &Runtime,
    file: &mut VolFile,
    spec: &RepoRemoveSpec,
) -> Result<Vec<JsonPathAction>, ErrCtx> {
    if !file.is_idle() {
        return pragma_err!("cannot remove while there is an open transaction");
    }
    let repo = repo_for_file(file)?;
    if let Some(path) = spec.path.as_deref() {
        stage_repo_remove_path(runtime, file, &repo, path, spec.cached)
    } else {
        let key = repo.file_key(&file.tag)?;
        Ok(vec![stage_repo_remove_key(
            runtime,
            file,
            &repo,
            &key,
            spec.cached,
        )?])
    }
}

fn stage_repo_remove_path(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    path: &Path,
    cached: bool,
) -> Result<Vec<JsonPathAction>, ErrCtx> {
    let physical_path = repo_input_path(repo, path);
    match std::fs::symlink_metadata(&physical_path) {
        Ok(metadata) if metadata.is_dir() => {
            let directory_key = repo_directory_key(repo, &physical_path)?;
            let removed = stage_repo_remove_directory(runtime, file, repo, &directory_key, cached)?;
            if removed.is_empty() {
                Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
                    if directory_key.is_empty() {
                        ".".to_string()
                    } else {
                        directory_key
                    },
                )))
            } else {
                Ok(removed)
            }
        }
        Ok(metadata) if metadata.is_file() => {
            let (key, _) = repo_physical_path_arg(repo, path)?;
            Ok(vec![stage_repo_remove_key(
                runtime, file, repo, &key, cached,
            )?])
        }
        Ok(_) => Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular file or directory",
                physical_path.display()
            )
            .into(),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let key = repo.file_key(&physical_path)?;
            Ok(vec![stage_repo_remove_key(
                runtime, file, repo, &key, cached,
            )?])
        }
        Err(err) => Err(err.into()),
    }
}

fn stage_repo_remove_directory(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    directory_key: &str,
    cached: bool,
) -> Result<Vec<JsonPathAction>, ErrCtx> {
    let keys = tracked_repo_keys_under_directory(repo, directory_key)?;
    let mut removed = Vec::with_capacity(keys.len());
    for key in keys {
        removed.push(stage_repo_remove_key(runtime, file, repo, &key, cached)?);
    }
    Ok(removed)
}

fn tracked_repo_keys_under_directory(
    repo: &Repository,
    directory_key: &str,
) -> Result<Vec<String>, ErrCtx> {
    let mut keys = BTreeSet::new();
    for key in repo
        .index_files()?
        .keys()
        .chain(repo.index_artifacts()?.keys())
    {
        if repo_key_is_under_directory(key, directory_key) {
            keys.insert(key.clone());
        }
    }
    Ok(keys.into_iter().collect())
}

fn repo_key_is_under_directory(key: &str, directory_key: &str) -> bool {
    directory_key.is_empty()
        || key == directory_key
        || key
            .strip_prefix(directory_key)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn repo_directory_key(repo: &Repository, path: &Path) -> Result<String, ErrCtx> {
    let directory = std::fs::canonicalize(path)?;
    if !directory.starts_with(repo.worktree()) {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathOutsideWorktree {
            path: directory,
            worktree: repo.worktree().to_path_buf(),
        }));
    }
    let relative = directory.strip_prefix(repo.worktree()).map_err(|_| {
        ErrCtx::Repo(graft::repo::RepoErr::PathOutsideWorktree {
            path: directory.clone(),
            worktree: repo.worktree().to_path_buf(),
        })
    })?;
    relative
        .to_str()
        .map(|path| path.replace('\\', "/"))
        .ok_or_else(|| ErrCtx::Repo(graft::repo::RepoErr::NonUtf8Path(relative.to_path_buf())))
}

fn stage_repo_remove_key(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    key: &str,
    cached: bool,
) -> Result<JsonPathAction, ErrCtx> {
    if cached {
        return stage_repo_remove_cached_key(repo, key);
    }

    let current_key = repo.file_key(&file.tag)?;
    if key == current_key {
        let physical_path = repo.worktree().join(key);
        let action = if repo.head_file(&file.tag)?.is_some() {
            let entry = repo.stage_file_removal(&file.tag)?;
            json_path_action(
                entry.path,
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
                "staged",
            )
        } else if let Some(artifact) = repo.head_artifact(&file.tag)? {
            let entry = repo.stage_file_removal(&file.tag)?;
            json_path_action(
                entry.path,
                artifact_checkout_path_kind(&artifact),
                artifact_checkout_path_storage(&artifact),
                "staged",
            )
        } else if repo.index_has_entry(&physical_path)? {
            let (kind, storage) = index_path_descriptor(repo, key)?;
            let path = repo.restore_index_path_from_head(&physical_path)?;
            json_path_action(path, kind, storage, "removed")
        } else {
            return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
                key.to_string(),
            )));
        };
        let volume = runtime.volume_open(None, None, None)?;
        file.switch_volume(&volume.vid)?;
        return Ok(action);
    }

    let physical_path = repo.worktree().join(key);
    if repo.head_file(&physical_path)?.is_some() {
        remove_physical_sqlite_file(repo, key, &physical_path)?;
        let entry = repo.stage_file_removal(&physical_path)?;
        return Ok(json_path_action(
            entry.path,
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
            "staged",
        ));
    }
    if let Some(artifact) = repo.head_artifact(&physical_path)? {
        remove_physical_artifact_file(&physical_path)?;
        let entry = repo.stage_file_removal(&physical_path)?;
        return Ok(json_path_action(
            entry.path,
            artifact_checkout_path_kind(&artifact),
            artifact_checkout_path_storage(&artifact),
            "staged",
        ));
    }
    if repo.index_has_entry(&physical_path)? {
        let (kind, storage) = index_path_descriptor(repo, key)?;
        remove_physical_artifact_file(&physical_path)?;
        let path = repo.restore_index_path_from_head(&physical_path)?;
        return Ok(json_path_action(path, kind, storage, "removed"));
    }

    Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
        key.to_string(),
    )))
}

fn stage_repo_remove_cached_key(repo: &Repository, key: &str) -> Result<JsonPathAction, ErrCtx> {
    let physical_path = repo.worktree().join(key);
    if repo.head_file(&physical_path)?.is_some() {
        let entry = repo.stage_file_removal_key(key)?;
        return Ok(json_path_action(
            entry.path,
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
            "staged",
        ));
    }

    if let Some(artifact) = repo.head_artifact(&physical_path)? {
        let entry = repo.stage_file_removal_key(key)?;
        return Ok(json_path_action(
            entry.path,
            artifact_checkout_path_kind(&artifact),
            artifact_checkout_path_storage(&artifact),
            "staged",
        ));
    }

    if repo.index_has_key(key)? {
        let (kind, storage) = index_path_descriptor(repo, key)?;
        let path = repo.restore_index_key_from_head(key)?;
        return Ok(json_path_action(path, kind, storage, "removed"));
    }

    Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
        key.to_string(),
    )))
}

fn index_path_descriptor(
    repo: &Repository,
    key: &str,
) -> Result<(RepoTrackedPathKind, RepoPathStorage), ErrCtx> {
    if repo.index_files()?.contains_key(key) {
        return Ok((
            RepoTrackedPathKind::SqliteDatabase,
            RepoPathStorage::SqliteSnapshot,
        ));
    }
    if let Some(artifact) = repo.index_artifacts()?.get(key) {
        return Ok((
            artifact_checkout_path_kind(artifact),
            artifact_checkout_path_storage(artifact),
        ));
    }
    Ok((RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline))
}

fn format_removed_paths(paths: &[JsonPathAction]) -> String {
    match paths {
        [path] => format!("Removed {}", path.path),
        paths => {
            let mut output = format!("Removed {} paths", paths.len());
            for path in paths {
                output.push_str("\n  ");
                output.push_str(&path.path);
            }
            output
        }
    }
}

fn remove_physical_sqlite_file(repo: &Repository, key: &str, path: &Path) -> Result<(), ErrCtx> {
    if repo.head_file(path)?.is_none() {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotTracked(
            key.to_string(),
        )));
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(ErrCtx::PragmaErr(
                    format!(
                        "path `{}` is not a regular SQLite database file",
                        path.display()
                    )
                    .into(),
                ));
            }
            std::fs::remove_file(path)?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    Ok(())
}

fn remove_physical_artifact_file(path: &Path) -> Result<(), ErrCtx> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(ErrCtx::PragmaErr(
                    format!("path `{}` is not a regular file", path.display()).into(),
                ));
            }
            std::fs::remove_file(path)?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    Ok(())
}

fn stage_physical_sqlite_file(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    path: &Path,
) -> Result<graft::repo::index::IndexEntry, ErrCtx> {
    let state = import_physical_sqlite_file_state(runtime, path)?;
    let entry = repo.stage_file_state_path(repo.worktree().join(key), state)?;
    Ok(entry)
}

fn import_physical_sqlite_file_state(
    runtime: &Runtime,
    path: &Path,
) -> Result<CommitFileState, ErrCtx> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{}` is not a regular SQLite database file",
                path.display()
            )
            .into(),
        ));
    }

    if metadata.len() < 100 {
        return Err(ErrCtx::PragmaErr(
            format!("path `{}` is not a SQLite database", path.display()).into(),
        ));
    }

    let mut input = File::open(path)?;
    let mut header = [0_u8; 100];
    input.read_exact(&mut header)?;
    if &header[..SQLITE_DATABASE_MAGIC.len()] != SQLITE_DATABASE_MAGIC {
        return Err(ErrCtx::PragmaErr(
            format!("path `{}` is not a SQLite database", path.display()).into(),
        ));
    }

    let sqlite_page_size = sqlite_page_size_from_header(&header);
    let graft_page_size = PAGESIZE.as_usize() as u32;
    if sqlite_page_size != graft_page_size {
        return Err(ErrCtx::PragmaErr(format!(
            "can only add SQLite databases with {graft_page_size}-byte pages directly; \
             `{}` uses {sqlite_page_size}-byte pages. Use VACUUM INTO with the Graft VFS to import it.",
            path.display()
        ).into()));
    }

    let page_size = PAGESIZE.as_usize();
    if metadata.len() % page_size as u64 != 0 {
        return Err(ErrCtx::PragmaErr(
            format!(
                "SQLite database `{}` is not an even multiple of {page_size} bytes",
                path.display()
            )
            .into(),
        ));
    }

    let page_count = metadata.len() / page_size as u64;
    let page_count_u32 = u32::try_from(page_count).map_err(|_| {
        ErrCtx::PragmaErr(
            format!(
                "SQLite database `{}` has too many pages to import",
                path.display()
            )
            .into(),
        )
    })?;

    let volume = runtime.volume_open(None, None, None)?;
    let vid = volume.vid;
    let mut writer = runtime.volume_writer(vid.clone())?;
    let mut page_bytes = vec![0_u8; page_size];
    let mut input = File::open(path)?;
    for page_number in 1..=page_count_u32 {
        input.read_exact(&mut page_bytes)?;
        let page = Page::try_from(page_bytes.as_slice()).map_err(|err| {
            ErrCtx::PragmaErr(format!("invalid SQLite page in `{}`: {err}", path.display()).into())
        })?;
        let pageidx = PageIdx::try_from(page_number).map_err(|err| {
            ErrCtx::PragmaErr(
                format!("invalid SQLite page index in `{}`: {err}", path.display()).into(),
            )
        })?;
        writer.write_page(pageidx, page)?;
    }
    let reader = writer.commit()?;
    Ok(CommitFileState {
        volume: vid,
        snapshot: repo_snapshot_with_commit_hashes(runtime, reader.snapshot())?,
    })
}

fn sqlite_page_size_from_header(header: &[u8; 100]) -> u32 {
    let raw = u16::from_be_bytes([header[16], header[17]]);
    if raw == 1 { 65_536 } else { raw as u32 }
}

fn repo_diff_for_spec(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    spec: RepoDiffSpec,
) -> Result<RepoDiff, ErrCtx> {
    let kind = spec.kind;
    let mut diff = match spec.target {
        RepoDiffTarget::Worktree { path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            let current_key = repo.file_key(&file.tag)?;
            if let Some(path) = path.as_deref()
                && path != current_key
            {
                let (key, physical_path) = repo_physical_path_arg(repo, Path::new(path))?;
                match std::fs::symlink_metadata(&physical_path) {
                    Ok(metadata) if metadata.file_type().is_dir() => {
                        repo_worktree_diff_for_filter(runtime, file, repo, None, &key)
                    }
                    Ok(metadata) if !metadata.file_type().is_file() => Err(ErrCtx::PragmaErr(
                        format!("path `{}` is not a regular file", physical_path.display()).into(),
                    )),
                    Ok(_) if is_sqlite_database_path(&physical_path)? => {
                        let state = import_physical_sqlite_file_state(runtime, &physical_path)?;
                        let expected = repo.index_files()?.get(&key).cloned();
                        let state = if let Some(expected) = expected
                            && repo_file_state_content_eq(runtime, &state, &expected)?
                        {
                            expected
                        } else {
                            state
                        };
                        Ok(repo.diff_worktree_file(&physical_path, state, Some(&key))?)
                    }
                    Ok(_) => Ok(repo.diff_worktree_artifact(&physical_path, Some(&key))?),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        if repo.index_artifact(&physical_path)?.is_some()
                            || repo.head_artifact(&physical_path)?.is_some()
                        {
                            Ok(repo.diff_worktree_artifact_removal(&physical_path, Some(&key))?)
                        } else {
                            Ok(repo.diff_worktree_file_removal(&physical_path, Some(&key))?)
                        }
                    }
                    Err(err) => Err(err.into()),
                }
            } else {
                let state = current_repo_file_state(runtime, file)?;
                Ok(repo.diff_worktree_file(&file.tag, state, path.as_deref())?)
            }
        }
        RepoDiffTarget::Staged { path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            Ok(repo.diff_staged(path.as_deref())?)
        }
        RepoDiffTarget::RevisionToWorktree { rev, path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            let current_key = repo.file_key(&file.tag)?;
            if let Some(path) = path.as_deref()
                && path != current_key
            {
                let (key, physical_path) = repo_physical_path_arg(repo, Path::new(path))?;
                match std::fs::symlink_metadata(&physical_path) {
                    Ok(metadata) if metadata.file_type().is_dir() => {
                        repo_worktree_diff_for_filter(runtime, file, repo, Some(&rev), &key)
                    }
                    Ok(metadata) if !metadata.file_type().is_file() => Err(ErrCtx::PragmaErr(
                        format!("path `{}` is not a regular file", physical_path.display()).into(),
                    )),
                    Ok(_) if is_sqlite_database_path(&physical_path)? => {
                        let state = import_physical_sqlite_file_state(runtime, &physical_path)?;
                        let from_id = repo.resolve_revision(&rev)?;
                        let expected = repo.read_commit(&from_id)?.files.get(&key).cloned();
                        let state = if let Some(expected) = expected
                            && repo_file_state_content_eq(runtime, &state, &expected)?
                        {
                            expected
                        } else {
                            state
                        };
                        Ok(repo.diff_revision_to_worktree_file(
                            &rev,
                            &physical_path,
                            state,
                            Some(&key),
                        )?)
                    }
                    Ok(_) => Ok(repo.diff_revision_to_worktree_artifact(
                        &rev,
                        &physical_path,
                        Some(&key),
                    )?),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        if repo.artifact_from_revision(&rev, &physical_path)?.is_some() {
                            Ok(repo.diff_revision_to_worktree_artifact_removal(
                                &rev,
                                &physical_path,
                                Some(&key),
                            )?)
                        } else {
                            Ok(repo.diff_revision_to_worktree_file_removal(
                                &rev,
                                &physical_path,
                                Some(&key),
                            )?)
                        }
                    }
                    Err(err) => Err(err.into()),
                }
            } else {
                let state = current_repo_file_state(runtime, file)?;
                Ok(repo.diff_revision_to_worktree_file(&rev, &file.tag, state, path.as_deref())?)
            }
        }
        RepoDiffTarget::Revisions { from, to, path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            Ok(repo.diff_revisions(&from, &to, path.as_deref())?)
        }
    }?;
    filter_repo_diff_by_kind(&mut diff, kind);
    Ok(diff)
}

fn filter_repo_diff_by_kind(diff: &mut RepoDiff, kind: Option<RepoTrackedPathKind>) {
    let Some(kind) = kind else {
        return;
    };
    diff.files.retain(|file| file.kind == kind);
    diff.artifacts.retain(|artifact| artifact.kind == kind);
    diff.refresh_paths();
}

fn repo_worktree_diff_for_filter(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    rev: Option<&str>,
    filter: &str,
) -> Result<RepoDiff, ErrCtx> {
    let from = if let Some(rev) = rev {
        repo.resolve_revision(rev)?
    } else {
        "index".to_string()
    };
    let mut diff = RepoDiff {
        from: from.clone(),
        to: "worktree".to_string(),
        paths: Vec::new(),
        files: Vec::new(),
        artifacts: Vec::new(),
    };
    let current_key = repo.file_key(&file.tag)?;
    let index_files = repo.index_files()?;
    let index_artifacts = repo.index_artifacts()?;
    let mut file_keys = BTreeSet::new();
    let mut artifact_keys = BTreeSet::new();

    if let Some(_) = rev {
        let commit = repo.read_commit(&from)?;
        file_keys.extend(
            commit
                .files
                .keys()
                .filter(|key| repo_key_matches_filter(key, filter))
                .cloned(),
        );
        artifact_keys.extend(
            commit
                .artifacts
                .keys()
                .filter(|key| repo_key_matches_filter(key, filter))
                .cloned(),
        );
    }
    file_keys.extend(
        index_files
            .keys()
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    artifact_keys.extend(
        index_artifacts
            .keys()
            .filter(|key| repo_key_matches_filter(key, filter))
            .cloned(),
    );
    if repo_key_matches_filter(&current_key, filter) {
        file_keys.insert(current_key.clone());
    }

    for key in file_keys {
        let physical_path = repo.worktree().join(&key);
        let path_diff = if key == current_key {
            let state = current_repo_file_state(runtime, file)?;
            if let Some(rev) = rev {
                repo.diff_revision_to_worktree_file(rev, &file.tag, state, Some(&key))?
            } else {
                repo.diff_worktree_file(&file.tag, state, Some(&key))?
            }
        } else {
            match std::fs::symlink_metadata(&physical_path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    if !is_sqlite_database_path(&physical_path)? {
                        continue;
                    }
                    let state = import_physical_sqlite_file_state(runtime, &physical_path)?;
                    let expected = if let Some(rev) = rev {
                        let from_id = repo.resolve_revision(rev)?;
                        repo.read_commit(&from_id)?.files.get(&key).cloned()
                    } else {
                        index_files.get(&key).cloned()
                    };
                    let state = if let Some(expected) = expected
                        && repo_file_state_content_eq(runtime, &state, &expected)?
                    {
                        expected
                    } else {
                        state
                    };
                    if let Some(rev) = rev {
                        repo.diff_revision_to_worktree_file(rev, &physical_path, state, Some(&key))?
                    } else {
                        repo.diff_worktree_file(&physical_path, state, Some(&key))?
                    }
                }
                Ok(_) => continue,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(rev) = rev {
                        repo.diff_revision_to_worktree_file_removal(
                            rev,
                            &physical_path,
                            Some(&key),
                        )?
                    } else {
                        repo.diff_worktree_file_removal(&physical_path, Some(&key))?
                    }
                }
                Err(err) => return Err(err.into()),
            }
        };
        append_repo_diff(&mut diff, path_diff);
    }

    for key in artifact_keys {
        let physical_path = repo.worktree().join(&key);
        let path_diff = match std::fs::symlink_metadata(&physical_path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                if let Some(rev) = rev {
                    repo.diff_revision_to_worktree_artifact(rev, &physical_path, Some(&key))?
                } else {
                    repo.diff_worktree_artifact(&physical_path, Some(&key))?
                }
            }
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Some(rev) = rev {
                    repo.diff_revision_to_worktree_artifact_removal(
                        rev,
                        &physical_path,
                        Some(&key),
                    )?
                } else {
                    repo.diff_worktree_artifact_removal(&physical_path, Some(&key))?
                }
            }
            Err(err) => return Err(err.into()),
        };
        append_repo_diff(&mut diff, path_diff);
    }

    Ok(diff)
}

fn append_repo_diff(target: &mut RepoDiff, mut source: RepoDiff) {
    target.files.append(&mut source.files);
    target.artifacts.append(&mut source.artifacts);
    target.refresh_paths();
}

fn repo_key_matches_filter(key: &str, filter: &str) -> bool {
    filter.is_empty()
        || key == filter
        || key
            .strip_prefix(filter)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn repo_status_for_file(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
) -> Result<RepoStatus, ErrCtx> {
    let mut status = repo.status()?;
    let current_key = repo.file_key(&file.tag)?;
    let tracked = match repo.index_files() {
        Ok(tracked) => tracked,
        Err(graft::repo::RepoErr::UnresolvedConflicts) => return Ok(status),
        Err(err) => return Err(err.into()),
    };
    let tracked_artifacts = match repo.index_artifacts() {
        Ok(tracked) => tracked,
        Err(graft::repo::RepoErr::UnresolvedConflicts) => return Ok(status),
        Err(err) => return Err(err.into()),
    };
    for change in &mut status.unstaged_changes {
        if change.path == current_key {
            change.kind = RepoTrackedPathKind::SqliteDatabase;
            change.storage = RepoPathStorage::SqliteSnapshot;
        }
    }
    status.unstaged_changes.retain(|change| {
        change.path == current_key
            || tracked.contains_key(&change.path)
            || tracked_artifacts.contains_key(&change.path)
            || (change.change == RepoWorktreeChangeKind::Untracked
                && should_report_untracked_status_path(repo))
    });
    for (key, expected_state) in tracked {
        if key == current_key
            || status
                .unstaged_changes
                .iter()
                .any(|change| change.path == key)
        {
            continue;
        }

        let physical_path = repo.worktree().join(&key);
        let change = match std::fs::symlink_metadata(&physical_path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    continue;
                }
                if !is_sqlite_database_path(&physical_path)? {
                    continue;
                }
                let state = import_physical_sqlite_file_state(runtime, &physical_path)?;
                if repo_file_state_content_eq(runtime, &state, &expected_state)? {
                    None
                } else {
                    Some(RepoWorktreeChangeKind::Modified)
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Some(RepoWorktreeChangeKind::Deleted)
            }
            Err(err) => return Err(err.into()),
        };

        if let Some(change) = change {
            status
                .unstaged_changes
                .push(graft::repo::RepoWorktreeChange {
                    path: key,
                    change,
                    kind: RepoTrackedPathKind::SqliteDatabase,
                    storage: RepoPathStorage::SqliteSnapshot,
                });
        }
    }
    status.unstaged_changes.sort_by(|a, b| a.path.cmp(&b.path));
    status.unstaged = status
        .unstaged_changes
        .iter()
        .map(|change| change.path.clone())
        .collect();
    status.refresh_summary_flags();
    Ok(status)
}

fn should_report_untracked_status_path(repo: &Repository) -> bool {
    repo.worktree().file_name().and_then(|name| name.to_str()) != Some(".eidos")
}

fn repo_has_work_in_progress_for_file(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
) -> Result<bool, ErrCtx> {
    let status = repo_status_for_file(runtime, file, repo)?;
    let current_key = repo.file_key(&file.tag)?;
    let has_blocking_unstaged = status.unstaged_changes.iter().any(|change| {
        change.change != RepoWorktreeChangeKind::Untracked || change.path == current_key
    });
    Ok(has_blocking_unstaged
        || !status.staged.is_empty()
        || !status.conflicted.is_empty()
        || status.merge_head.is_some())
}

fn ensure_checkout_plan_preserves_untracked_paths(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    plan: &CheckoutPlan,
) -> Result<(), ErrCtx> {
    let keys = plan
        .files
        .keys()
        .chain(plan.artifacts.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    ensure_checkout_keys_preserve_untracked_paths(runtime, file, repo, &keys)
}

fn ensure_checkout_key_preserves_untracked_path(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    key: &str,
) -> Result<(), ErrCtx> {
    let keys = BTreeSet::from([key.to_string()]);
    ensure_checkout_keys_preserve_untracked_paths(runtime, file, repo, &keys)
}

fn ensure_checkout_keys_preserve_untracked_paths(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    keys: &BTreeSet<String>,
) -> Result<(), ErrCtx> {
    if keys.is_empty() {
        return Ok(());
    }
    let status = repo_status_for_file(runtime, file, repo)?;
    let current_key = repo.file_key(&file.tag)?;
    let overwritten = status
        .unstaged_changes
        .iter()
        .filter(|change| {
            change.change == RepoWorktreeChangeKind::Untracked
                && change.path != current_key
                && keys.contains(&change.path)
        })
        .map(|change| change.path.clone())
        .collect::<Vec<_>>();

    if overwritten.is_empty() {
        return Ok(());
    }

    pragma_err!(format!(
        "cannot checkout because untracked paths would be overwritten: {}",
        overwritten.join(", ")
    ))
}

fn repo_file_state_content_eq(
    runtime: &Runtime,
    left: &CommitFileState,
    right: &CommitFileState,
) -> Result<bool, ErrCtx> {
    if left.snapshot.page_count != right.snapshot.page_count {
        return Ok(false);
    }
    let left_snapshot = left.snapshot.to_snapshot();
    let right_snapshot = right.snapshot.to_snapshot();
    Ok(runtime.snapshot_checksum(&left_snapshot)? == runtime.snapshot_checksum(&right_snapshot)?)
}

fn staged_commit_table_summary(
    runtime: &Runtime,
    repo: &Repository,
) -> Result<Vec<CommitTableSummary>, ErrCtx> {
    let diff = repo.diff_staged(None)?;
    let mut by_name = BTreeMap::<String, CommitTableSummary>::new();
    for file in &diff.files {
        let summaries = repo_file_table_summary(runtime, file)?;
        for summary in summaries {
            merge_table_summary(&mut by_name, summary);
        }
    }
    Ok(by_name.into_values().collect())
}

fn repo_file_table_summary(
    runtime: &Runtime,
    file: &graft::repo::RepoFileDiff,
) -> Result<Vec<CommitTableSummary>, ErrCtx> {
    match (&file.from, &file.to) {
        (Some(from), Some(to)) => {
            let from_snapshot = from.snapshot.to_snapshot();
            let to_snapshot = to.snapshot.to_snapshot();
            if from_snapshot.is_empty() {
                return snapshot_table_summary(
                    runtime,
                    &to_snapshot,
                    SnapshotSummaryMode::Inserted,
                );
            }
            if to_snapshot.is_empty() {
                return snapshot_table_summary(
                    runtime,
                    &from_snapshot,
                    SnapshotSummaryMode::Deleted,
                );
            }
            let diff = crate::row_level_diff::row_level_diff_snapshots(
                runtime,
                &from_snapshot,
                &to_snapshot,
            )
            .map_err(|e| ErrCtx::PragmaErr(format!("Diff error: {e:?}").into()))?;
            Ok(diff
                .table_changes
                .iter()
                .filter_map(|table| {
                    let (inserts, deletes, updates) = count_changes_json(&table.changes);
                    table_summary(table.table_name.clone(), inserts, deletes, updates)
                })
                .collect())
        }
        (None, Some(to)) => snapshot_table_summary(
            runtime,
            &to.snapshot.to_snapshot(),
            SnapshotSummaryMode::Inserted,
        ),
        (Some(from), None) => snapshot_table_summary(
            runtime,
            &from.snapshot.to_snapshot(),
            SnapshotSummaryMode::Deleted,
        ),
        (None, None) => Ok(Vec::new()),
    }
}

#[derive(Debug, Clone, Copy)]
enum SnapshotSummaryMode {
    Inserted,
    Deleted,
}

fn snapshot_table_summary(
    runtime: &Runtime,
    snapshot: &graft::snapshot::Snapshot,
    mode: SnapshotSummaryMode,
) -> Result<Vec<CommitTableSummary>, ErrCtx> {
    if snapshot.is_empty() {
        return Ok(Vec::new());
    }

    let volume = runtime.volume_from_snapshot(snapshot)?;
    let vid = volume.vid.clone();
    let result = snapshot_table_summary_checked_out(runtime, &vid, mode);
    let _ = runtime.volume_delete(&vid);
    result
}

fn snapshot_table_summary_checked_out(
    runtime: &Runtime,
    vid: &VolumeId,
    mode: SnapshotSummaryMode,
) -> Result<Vec<CommitTableSummary>, ErrCtx> {
    let reader = runtime.volume_reader(vid.clone())?;
    let scanner = crate::sqlite_parse::TableScanner::new(&reader)
        .map_err(|e| ErrCtx::PragmaErr(format!("Parse error: {e:?}").into()))?;
    let master = scanner
        .read_master_table()
        .map_err(|e| ErrCtx::PragmaErr(format!("Schema error: {e:?}").into()))?;
    let mut summaries = Vec::new();
    let ignored_tables = crate::row_level_diff::ignored_row_diff_tables(&master, &[]);

    for entry in master {
        if !crate::row_level_diff::is_diffable_table(&entry, &ignored_tables) {
            continue;
        }
        let row_count = crate::sqlite_parse::read_all_rows(&reader, entry.root_page)
            .map_err(|e| ErrCtx::PragmaErr(format!("Table read error: {e:?}").into()))?
            .len();
        let summary = match mode {
            SnapshotSummaryMode::Inserted => table_summary(entry.name, row_count, 0, 0),
            SnapshotSummaryMode::Deleted => table_summary(entry.name, 0, row_count, 0),
        };
        if let Some(summary) = summary {
            summaries.push(summary);
        }
    }

    Ok(summaries)
}

fn table_summary(
    name: String,
    inserts: usize,
    deletes: usize,
    updates: usize,
) -> Option<CommitTableSummary> {
    if name.is_empty() || inserts + deletes + updates == 0 {
        None
    } else {
        Some(CommitTableSummary { name, inserts, deletes, updates })
    }
}

fn merge_table_summary(
    by_name: &mut BTreeMap<String, CommitTableSummary>,
    summary: CommitTableSummary,
) {
    by_name
        .entry(summary.name.clone())
        .and_modify(|entry| {
            entry.inserts += summary.inserts;
            entry.deletes += summary.deletes;
            entry.updates += summary.updates;
        })
        .or_insert(summary);
}

fn is_sqlite_database_path(path: &Path) -> Result<bool, ErrCtx> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    let mut magic = [0_u8; SQLITE_DATABASE_MAGIC.len()];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == SQLITE_DATABASE_MAGIC),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn is_sqlite_sidecar_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal")
        })
}

fn current_repo_file_state(runtime: &Runtime, file: &VolFile) -> Result<CommitFileState, ErrCtx> {
    let snapshot = file.snapshot_or_latest()?;
    Ok(CommitFileState {
        volume: file.vid.clone(),
        snapshot: repo_snapshot_with_commit_hashes(runtime, &snapshot)?,
    })
}

fn repo_snapshot_with_commit_hashes(
    runtime: &Runtime,
    snapshot: &graft::snapshot::Snapshot,
) -> Result<RepoSnapshot, ErrCtx> {
    let mut ranges = Vec::new();
    for range in snapshot.iter() {
        let mut commits = Vec::new();
        for lsn in range.lsns.iter() {
            let commit_hash =
                repo_storage_commit_hash(runtime, &range.log, lsn)?.ok_or_else(|| {
                    ErrCtx::PragmaErr(
                        format!(
                            "snapshot references missing storage commit {:?}/{}",
                            range.log, lsn
                        )
                        .into(),
                    )
                })?;
            commits.push(RepoStorageCommit { lsn, commit_hash });
        }
        ranges.push(RepoLogRange {
            log: range.log.clone(),
            start: *range.lsns.start(),
            end: *range.lsns.end(),
            commits,
        });
    }
    Ok(RepoSnapshot { page_count: snapshot.page_count, ranges })
}

fn conflict_side_state(
    repo: &Repository,
    key: &str,
    side: ResolveSide,
) -> Result<RepoConflictSideState, ErrCtx> {
    let Some(stage) = side.index_stage() else {
        return Err(ErrCtx::PragmaErr(
            "manual resolution does not have an index conflict stage".into(),
        ));
    };
    let index = repo.read_index()?;
    if !index
        .entries
        .iter()
        .any(|entry| entry.path == key && entry.stage != graft::repo::index::IndexStage::Normal)
    {
        return Err(ErrCtx::Repo(graft::repo::RepoErr::PathNotConflicted(
            key.to_string(),
        )));
    }
    let Some(entry) = index
        .entries
        .iter()
        .find(|entry| entry.path == key && entry.stage == stage)
    else {
        return Ok(RepoConflictSideState::Deleted);
    };
    if let Some(file) = &entry.file {
        Ok(RepoConflictSideState::SqliteDatabase(file.clone()))
    } else if let Some(artifact) = &entry.artifact {
        Ok(RepoConflictSideState::Artifact(artifact.clone()))
    } else {
        Ok(RepoConflictSideState::Deleted)
    }
}

fn resolve_repo_conflict_for_file(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: RepoResolveSpec,
) -> Result<RepoResolveConflictOutcome, ErrCtx> {
    let path = spec.path.unwrap_or_else(|| PathBuf::from(&file.tag));
    let (key, physical_path) = repo_physical_path_arg(repo, &path)?;
    let (path_kind, path_storage) = conflict_path_descriptor(repo, &key)?;
    let current_key = repo.file_key(&file.tag)?;
    if let Some(row) = spec.row.as_ref() {
        let path = resolve_repo_row_conflict(
            runtime,
            file,
            repo,
            &key,
            &physical_path,
            &current_key,
            spec.side,
            row,
        )?;
        return Ok(RepoResolveConflictOutcome { path, path_kind, path_storage });
    }
    if matches!(spec.side, ResolveSide::Ours | ResolveSide::Theirs) {
        if let Some(state) = row_resolved_conflict_file_state(runtime, repo, &key, spec.side)? {
            if key == current_key {
                checkout_repo_file_state(runtime, file, &state, None)?;
            } else {
                checkout_repo_file_state_to_path(runtime, repo, &state, &physical_path, None)?;
            }
            let entry = repo.resolve_file_conflict(&physical_path, Some(state))?;
            clear_row_conflict_resolution_state(repo)?;
            return Ok(RepoResolveConflictOutcome {
                path: entry.path,
                path_kind,
                path_storage,
            });
        }
    }
    let state = match spec.side {
        ResolveSide::Ours | ResolveSide::Theirs => {
            match conflict_side_state(repo, &key, spec.side)? {
                RepoConflictSideState::SqliteDatabase(state) => {
                    if key == current_key {
                        checkout_repo_file_state(runtime, file, &state, None)?;
                    } else {
                        checkout_repo_file_state_to_path(
                            runtime,
                            repo,
                            &state,
                            &physical_path,
                            None,
                        )?;
                    }
                    Some(state)
                }
                RepoConflictSideState::Artifact(state) => {
                    if key == current_key {
                        let volume = runtime.volume_open(None, None, None)?;
                        file.switch_volume(&volume.vid)?;
                    }
                    repo.materialize_artifact_key(&key, &state)?;
                    let entry = repo.resolve_artifact_conflict(&physical_path, Some(state))?;
                    clear_row_conflict_resolution_state(repo)?;
                    return Ok(RepoResolveConflictOutcome {
                        path: entry.path,
                        path_kind,
                        path_storage,
                    });
                }
                RepoConflictSideState::Deleted => {
                    if key == current_key {
                        let volume = runtime.volume_open(None, None, None)?;
                        file.switch_volume(&volume.vid)?;
                    } else {
                        remove_materialized_repo_file(repo, &key)?;
                    }
                    None
                }
            }
        }
        ResolveSide::Manual if key == current_key => Some(current_repo_file_state(runtime, file)?),
        ResolveSide::Manual
            if physical_path.exists() && !is_sqlite_database_path(&physical_path)? =>
        {
            let entry = repo.resolve_artifact_conflict_from_path(&physical_path)?;
            clear_row_conflict_resolution_state(repo)?;
            return Ok(RepoResolveConflictOutcome {
                path: entry.path,
                path_kind,
                path_storage,
            });
        }
        ResolveSide::Manual if physical_path.exists() => {
            Some(import_physical_sqlite_file_state(runtime, &physical_path)?)
        }
        ResolveSide::Manual => None,
    };
    let entry = repo.resolve_file_conflict(&physical_path, state)?;
    clear_row_conflict_resolution_state(repo)?;
    Ok(RepoResolveConflictOutcome {
        path: entry.path,
        path_kind,
        path_storage,
    })
}

fn resolve_repo_row_conflict(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    key: &str,
    physical_path: &Path,
    current_key: &str,
    side: ResolveSide,
    row: &RepoResolveRowSpec,
) -> Result<String, ErrCtx> {
    if side == ResolveSide::Manual {
        return Err(ErrCtx::PragmaErr(
            "row conflict resolution requires `--ours` or `--theirs`".into(),
        ));
    }

    let status = repo.status()?;
    let mut resolution_state =
        read_row_conflict_resolution_state(repo, status.merge_head.as_deref())?;
    let Some((base, ours, theirs)) = current_file_conflict_states(repo, key)? else {
        return Err(ErrCtx::PragmaErr(
            format!("path `{key}` has no row conflict stages").into(),
        ));
    };
    let remote = repo_default_remote_store(repo);
    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
    if !plan.schema_conflicts().is_empty() || plan.has_opaque_changes() {
        return Err(ErrCtx::PragmaErr(
            "row conflict resolution is not available with schema or opaque conflicts".into(),
        ));
    }
    let requested_conflict = plan
        .analysis
        .conflicts
        .iter()
        .find(|conflict| conflict.table == row.table && conflict.rowid == row.rowid);
    let Some(requested_conflict) = requested_conflict else {
        return Err(ErrCtx::PragmaErr(
            format!(
                "path `{key}` has no row conflict for {} rowid={}",
                row.table, row.rowid
            )
            .into(),
        ));
    };
    if requested_conflict.reason == crate::row_merge::RowMergeConflictReason::SemanticKey {
        return Err(ErrCtx::PragmaErr(
            format!(
                "semantic key conflict for {} rowid={} requires manual file resolution",
                row.table, row.rowid
            )
            .into(),
        ));
    }

    resolution_state.rows.insert(
        row_conflict_resolution_key(key, &row.table, row.rowid),
        side.label().to_string(),
    );
    let merged = materialize_row_conflict_resolution_state(
        runtime,
        repo,
        key,
        &ours,
        &plan,
        &resolution_state,
    )?;
    if key == current_key {
        checkout_repo_file_state(runtime, file, &merged, None)?;
    } else {
        checkout_repo_file_state_to_path(runtime, repo, &merged, physical_path, None)?;
    }

    let unresolved = unresolved_row_conflict_count(key, &plan, &resolution_state);
    if unresolved == 0 {
        let entry = repo.resolve_file_conflict(physical_path, Some(merged))?;
        clear_row_conflict_resolution_state(repo)?;
        return Ok(entry.path);
    }

    write_row_conflict_resolution_state(repo, &resolution_state)?;
    Ok(key.to_string())
}

fn row_resolved_conflict_file_state(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    side: ResolveSide,
) -> Result<Option<CommitFileState>, ErrCtx> {
    let Some((base, ours, theirs)) = current_file_conflict_states(repo, key)? else {
        return Ok(None);
    };
    let remote = repo_default_remote_store(repo);
    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
    if !plan.analysis.has_conflicts()
        || !plan.schema_conflicts().is_empty()
        || plan.has_opaque_changes()
    {
        return Ok(None);
    }

    let (base_state, sql) = match side {
        ResolveSide::Ours => (&ours, plan.theirs_apply_sql()),
        ResolveSide::Theirs => (&theirs, plan.ours_apply_sql()),
        ResolveSide::Manual => return Ok(None),
    };
    materialize_row_auto_merge_state(runtime, repo, key, base_state, &sql).map(Some)
}

fn materialize_row_conflict_resolution_state(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    ours: &CommitFileState,
    plan: &crate::row_merge::RowMergePlan,
    resolution_state: &RowConflictResolutionState,
) -> Result<CommitFileState, ErrCtx> {
    let mut sql = plan.theirs_apply_sql();
    for conflict in &plan.analysis.conflicts {
        let selection_key = row_conflict_resolution_key(key, &conflict.table, conflict.rowid);
        let Some(selection) = resolution_state.rows.get(&selection_key) else {
            continue;
        };
        let Some(side) = row_merge_side_from_label(selection) else {
            continue;
        };
        let Some(row_sql) = plan.conflict_apply_sql(side, &conflict.table, conflict.rowid) else {
            return Err(ErrCtx::PragmaErr(
                format!(
                    "could not generate row resolution for {} rowid={}",
                    conflict.table, conflict.rowid
                )
                .into(),
            ));
        };
        sql.push('\n');
        sql.push_str(&row_sql);
    }
    materialize_row_auto_merge_state(runtime, repo, key, ours, &sql)
}

fn unresolved_row_conflict_count(
    key: &str,
    plan: &crate::row_merge::RowMergePlan,
    resolution_state: &RowConflictResolutionState,
) -> usize {
    plan.analysis
        .conflicts
        .iter()
        .filter(|conflict| {
            !resolution_state
                .rows
                .contains_key(&row_conflict_resolution_key(
                    key,
                    &conflict.table,
                    conflict.rowid,
                ))
        })
        .count()
}

fn row_merge_side_from_label(label: &str) -> Option<crate::row_merge::RowMergeSide> {
    match label {
        "ours" => Some(crate::row_merge::RowMergeSide::Ours),
        "theirs" => Some(crate::row_merge::RowMergeSide::Theirs),
        _ => None,
    }
}

fn row_merge_policy_for_repo(
    repo: &Repository,
) -> Result<crate::row_merge::RowMergePolicy, ErrCtx> {
    let config = repo.config()?;
    let mut policy = crate::row_merge::RowMergePolicy::default();
    policy.default_semantic_keys = config.merge.default_semantic_keys;
    policy.semantic_keys = config.merge.semantic_keys;
    for (subject, resolver) in config.merge.internal_resolvers {
        let Some(resolver) = crate::row_merge::RowMergeInternalResolver::from_str(&resolver) else {
            continue;
        };
        if internal_resolver_allowed_for_subject(&subject, resolver) {
            policy.internal_resolvers.insert(subject, resolver);
        }
    }
    for (operation, resolver) in config.merge.schema_resolvers {
        if let Some(resolver) = crate::row_merge::RowMergeSchemaResolver::from_str(&resolver) {
            policy.schema_resolvers.insert(operation, resolver);
        }
    }
    policy.generated_columns = config.merge.generated_columns;
    Ok(policy)
}

fn internal_resolver_allowed_for_subject(
    subject: &str,
    resolver: crate::row_merge::RowMergeInternalResolver,
) -> bool {
    match subject {
        "sqlite_sequence" => resolver == crate::row_merge::RowMergeInternalResolver::SequenceMax,
        "sqlite_stat1" | "sqlite_stat2" | "sqlite_stat3" | "sqlite_stat4" => {
            resolver == crate::row_merge::RowMergeInternalResolver::Rebuild
        }
        "index_btree" => resolver == crate::row_merge::RowMergeInternalResolver::Reindex,
        _ => false,
    }
}

fn plan_repo_snapshot_merge(
    runtime: &Runtime,
    repo: &Repository,
    base: &CommitFileState,
    ours: &CommitFileState,
    theirs: &CommitFileState,
) -> Result<crate::row_merge::RowMergePlan, ErrCtx> {
    let policy = row_merge_policy_for_repo(repo)?;
    Ok(crate::row_merge::plan_snapshot_merge_with_policy(
        runtime, base, ours, theirs, &policy,
    )?)
}

fn row_conflict_resolution_key(path: &str, table: &str, rowid: i64) -> String {
    format!("{path}\u{1f}{table}\u{1f}{rowid}")
}

fn row_conflict_resolution_state_path(repo: &Repository) -> PathBuf {
    repo.worktree()
        .join(".graft")
        .join("row-conflict-resolutions.json")
}

fn read_row_conflict_resolution_state(
    repo: &Repository,
    merge_head: Option<&str>,
) -> Result<RowConflictResolutionState, ErrCtx> {
    let path = row_conflict_resolution_state_path(repo);
    let state = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str::<RowConflictResolutionState>(&raw).map_err(|err| {
            ErrCtx::PragmaErr(
                format!(
                    "could not parse row conflict resolution state `{}`: {err}",
                    path.display()
                )
                .into(),
            )
        })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            RowConflictResolutionState::default()
        }
        Err(err) => return Err(err.into()),
    };
    let merge_head = merge_head.map(str::to_string);
    if state.merge_head == merge_head {
        Ok(state)
    } else {
        Ok(RowConflictResolutionState { merge_head, rows: BTreeMap::new() })
    }
}

fn write_row_conflict_resolution_state(
    repo: &Repository,
    state: &RowConflictResolutionState,
) -> Result<(), ErrCtx> {
    let path = row_conflict_resolution_state_path(repo);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(state).map_err(|err| {
        ErrCtx::PragmaErr(format!("could not encode row conflict resolution state: {err}").into())
    })?;
    std::fs::write(path, raw)?;
    Ok(())
}

fn clear_row_conflict_resolution_state(repo: &Repository) -> Result<(), ErrCtx> {
    match std::fs::remove_file(row_conflict_resolution_state_path(repo)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn reset_mode_label(mode: ResetMode) -> &'static str {
    match mode {
        ResetMode::Soft => "soft",
        ResetMode::Mixed => "mixed",
        ResetMode::Hard => "hard",
    }
}

fn repo_remote_branch(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
) -> Result<BranchUpstream, ErrCtx> {
    Ok(repo.default_remote_branch(remote.as_deref(), branch.as_deref())?)
}

fn repo_default_remote(repo: &Repository, remote: Option<String>) -> Result<String, ErrCtx> {
    Ok(repo.default_remote_branch(remote.as_deref(), None)?.remote)
}

fn repo_push_branches(
    repo: &Repository,
    remote: Option<String>,
    branch: Option<String>,
) -> Result<(String, String, String), ErrCtx> {
    let current_branch = repo
        .current_branch()?
        .ok_or_else(|| ErrCtx::PragmaErr("cannot push in detached HEAD".into()))?;
    let upstream = repo.default_remote_branch(remote.as_deref(), branch.as_deref())?;
    let local_branch = branch.unwrap_or(current_branch);
    Ok((upstream.remote, local_branch, upstream.branch))
}

fn format_fetch_all_outcome(outcome: &FetchAllOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    let commits: usize = outcome.branches.iter().map(|branch| branch.commits).sum();
    writeln!(
        &mut f,
        "Fetched {} ({} {}, {} new {})",
        outcome.remote,
        outcome.branches.len(),
        pluralize!(outcome.branches.len(), "branch"),
        commits,
        pluralize!(commits, "commit")
    )?;
    for branch in &outcome.branches {
        writeln!(
            &mut f,
            "  {}/{} at {} ({} new {})",
            branch.remote,
            branch.branch,
            &branch.head[..12],
            branch.commits,
            pluralize!(branch.commits, "commit")
        )?;
    }
    Ok(f)
}

fn format_push_all_outcome(outcome: &PushAllOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    let commits: usize = outcome.branches.iter().map(|branch| branch.commits).sum();
    let forced = outcome.branches.iter().any(|branch| branch.forced);
    writeln!(
        &mut f,
        "{} {} ({} {}, {} {})",
        if forced { "Force-pushed" } else { "Pushed" },
        outcome.remote,
        outcome.branches.len(),
        pluralize!(outcome.branches.len(), "branch"),
        commits,
        pluralize!(commits, "commit")
    )?;
    for branch in &outcome.branches {
        if branch.deleted {
            writeln!(
                &mut f,
                "  Deleted {}/{} (was {})",
                branch.remote,
                branch.remote_branch,
                &branch.head[..branch.head.len().min(12)]
            )?;
            continue;
        }
        writeln!(
            &mut f,
            "  {}{}/{} at {} ({} {})",
            if branch.forced { "+" } else { "" },
            branch.remote,
            branch.remote_branch,
            &branch.head[..12],
            branch.commits,
            pluralize!(branch.commits, "commit")
        )?;
    }
    Ok(f)
}

fn format_repo_diff(diff: &RepoDiff) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Diff {}..{}",
        &diff.from[..diff.from.len().min(12)],
        &diff.to[..diff.to.len().min(12)]
    )?;
    if diff.files.is_empty() && diff.artifacts.is_empty() {
        writeln!(&mut f, "No changes.")?;
        return Ok(f);
    }

    for file in &diff.files {
        let change = repo_file_change_label(file.change);
        writeln!(&mut f, "{change}: {}", file.path)?;
        if let Some(from) = &file.from {
            writeln!(
                &mut f,
                "  from: {} page(s), {} range(s)",
                from.snapshot.page_count,
                from.snapshot.ranges.len()
            )?;
        }
        if let Some(to) = &file.to {
            writeln!(
                &mut f,
                "  to:   {} page(s), {} range(s)",
                to.snapshot.page_count,
                to.snapshot.ranges.len()
            )?;
        }
    }
    for artifact in &diff.artifacts {
        let change = repo_file_change_label(artifact.change);
        writeln!(&mut f, "{change}: {}", artifact.path)?;
        if let Some(from) = &artifact.from {
            writeln!(
                &mut f,
                "  from: {} byte(s), {}, {}",
                from.size(),
                repo_artifact_state_label(from),
                from.content_hash()
            )?;
        }
        if let Some(to) = &artifact.to {
            writeln!(
                &mut f,
                "  to:   {} byte(s), {}, {}",
                to.size(),
                repo_artifact_state_label(to),
                to.content_hash()
            )?;
        }
    }
    Ok(f)
}

fn format_repo_row_diff(
    runtime: &Runtime,
    repo: &Repository,
    diff: &RepoDiff,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Row Diff {}..{}",
        &diff.from[..diff.from.len().min(12)],
        &diff.to[..diff.to.len().min(12)]
    )?;
    if diff.files.is_empty() {
        writeln!(&mut f, "No changes.")?;
        return Ok(f);
    }

    for file in &diff.files {
        let change = repo_file_change_label(file.change);
        writeln!(&mut f, "{change}: {}", file.path)?;
        let Some(row_diff) = repo_file_row_diff(runtime, repo, file)? else {
            writeln!(
                &mut f,
                "  Row diff unavailable for {} database snapshots.",
                change
            )?;
            continue;
        };
        write_indented(&mut f, &row_diff.to_report(), "  ")?;
    }
    Ok(f)
}

fn repo_file_change_label(change: RepoFileChange) -> &'static str {
    match change {
        RepoFileChange::Added => "added",
        RepoFileChange::Deleted => "deleted",
        RepoFileChange::Modified => "modified",
    }
}

fn repo_artifact_state_label(state: &CommitArtifactState) -> &'static str {
    if state.is_large() {
        "external payload"
    } else {
        "file"
    }
}

fn repo_file_row_diff(
    runtime: &Runtime,
    repo: &Repository,
    file: &graft::repo::RepoFileDiff,
) -> Result<Option<crate::row_level_diff::RowLevelDiff>, ErrCtx> {
    let (Some(from), Some(to)) = (&file.from, &file.to) else {
        return Ok(None);
    };
    let resolver = RepoSnapshotResolver::local_then_remote(
        runtime,
        repo_default_remote_store(repo),
        RepoSnapshotPurpose::Diff,
        SnapshotHashPolicy::AllowHydratedMismatch,
    );
    resolver.resolve_snapshot(&from.snapshot)?;
    resolver.resolve_snapshot(&to.snapshot)?;
    crate::row_level_diff::row_level_diff_snapshots(
        runtime,
        &from.snapshot.to_snapshot(),
        &to.snapshot.to_snapshot(),
    )
    .map(Some)
    .map_err(|err| ErrCtx::PragmaErr(format!("Row diff error for `{}`: {err:?}", file.path).into()))
}

fn repo_default_remote_store(repo: &Repository) -> Option<Arc<Remote>> {
    let remote = repo_default_remote(repo, None).ok()?;
    repo.remote_store(&remote).ok().map(Arc::new)
}

fn write_indented(out: &mut String, text: &str, prefix: &str) -> Result<(), ErrCtx> {
    for line in text.lines() {
        writeln!(out, "{prefix}{line}")?;
    }
    Ok(())
}

fn format_repo_show(commit: &graft::repo::CommitObject) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(&mut f, "commit {}", commit.id)?;
    if commit.parents.is_empty() {
        if let Some(parent) = &commit.parent {
            writeln!(&mut f, "parent {parent}")?;
        }
    } else {
        for parent in &commit.parents {
            writeln!(&mut f, "parent {parent}")?;
        }
    }
    if let Some(tree) = &commit.tree {
        writeln!(&mut f, "tree {tree}")?;
    }
    writeln!(&mut f, "date {}", format_unix_millis(commit.timestamp_ms))?;
    writeln!(&mut f)?;
    writeln!(&mut f, "    {}", commit.message)?;
    if !commit.files.is_empty() {
        writeln!(&mut f)?;
        writeln!(&mut f, "Files:")?;
        for (path, state) in &commit.files {
            writeln!(
                &mut f,
                "  {} ({} page(s), {} range(s))",
                path,
                state.snapshot.page_count,
                state.snapshot.ranges.len()
            )?;
        }
    }
    if !commit.artifacts.is_empty() {
        writeln!(&mut f)?;
        writeln!(&mut f, "Artifacts:")?;
        for (path, state) in &commit.artifacts {
            writeln!(
                &mut f,
                "  {} ({} byte(s), {}, {})",
                path,
                state.size(),
                repo_artifact_state_label(state),
                state.content_hash()
            )?;
        }
    }
    Ok(f)
}

fn format_repo_status(status: &RepoStatus) -> Result<String, ErrCtx> {
    let mut f = String::new();
    match &status.head {
        Head::Branch { name } => writeln!(&mut f, "On branch {name}")?,
        Head::Detached { commit } => writeln!(&mut f, "HEAD detached at {commit}")?,
    }
    if let Some(upstream) = &status.upstream {
        writeln!(&mut f, "Tracking: {}/{}", upstream.remote, upstream.branch)?;
    }
    writeln!(&mut f, "Repository: {}", status.worktree.display())?;
    writeln!(&mut f, "Format: v{}", status.repository_format_version)?;
    match &status.head_target {
        Some(target) => writeln!(&mut f, "HEAD: {target}")?,
        None => writeln!(&mut f, "No commits yet")?,
    }
    if let Some(merge_head) = &status.merge_head {
        writeln!(
            &mut f,
            "Merge in progress with {}",
            &merge_head[..merge_head.len().min(12)]
        )?;
    }
    if !status.conflicted.is_empty() {
        writeln!(&mut f, "Unmerged paths:")?;
        for path in &status.conflicted {
            writeln!(&mut f, "  {path}")?;
        }
    }
    if !status.staged.is_empty() {
        writeln!(&mut f, "Changes to be committed:")?;
        for path in &status.staged {
            writeln!(&mut f, "  {path}")?;
        }
    }
    if !status.unstaged_changes.is_empty() {
        writeln!(&mut f, "Changes not staged for commit.")?;
        writeln!(&mut f, "  (use 'pragma graft_add' to stage)")?;
        for change in &status.unstaged_changes {
            writeln!(
                &mut f,
                "  {}: {}",
                worktree_change_label(change.change),
                change.path
            )?;
        }
    } else if !status.unstaged.is_empty() {
        writeln!(&mut f, "Changes not staged for commit.")?;
        writeln!(&mut f, "  (use 'pragma graft_add' to stage)")?;
        for path in &status.unstaged {
            writeln!(&mut f, "  {path}")?;
        }
    }
    if status.unstaged.is_empty()
        && status.staged.is_empty()
        && status.conflicted.is_empty()
        && status.merge_head.is_none()
    {
        writeln!(&mut f, "Worktree clean.")?;
    }
    Ok(f)
}

fn worktree_change_label(change: RepoWorktreeChangeKind) -> &'static str {
    match change {
        RepoWorktreeChangeKind::Modified => "modified",
        RepoWorktreeChangeKind::Deleted => "deleted",
        RepoWorktreeChangeKind::Untracked => "untracked",
    }
}

fn format_repo_artifact_audit(audit: &RepoArtifactAudit) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if audit.ok() {
        writeln!(&mut f, "Repository artifact payloads OK.")?;
    } else {
        writeln!(&mut f, "Repository artifact payload issues:")?;
        for issue in &audit.issues {
            writeln!(
                &mut f,
                "  {}: {} ({})",
                repo_artifact_audit_issue_label(issue.kind),
                issue.path,
                issue.message
            )?;
        }
    }
    writeln!(&mut f, "Artifacts: {}", audit.artifacts)?;
    writeln!(&mut f, "External payloads: {}", audit.external_payloads)?;
    Ok(f)
}

fn format_repo_artifact_repair(outcome: &RepoArtifactRepairOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Repaired repository artifact payloads from {}.",
        outcome.remote
    )?;
    writeln!(&mut f, "Fetched objects: {}", outcome.fetched_objects)?;
    writeln!(
        &mut f,
        "Fetched external payloads: {}",
        outcome.fetched_external_payloads
    )?;
    writeln!(&mut f, "Issues before: {}", outcome.before.issues.len())?;
    writeln!(&mut f, "Issues after: {}", outcome.after.issues.len())?;
    if !outcome.after.ok() {
        writeln!(&mut f, "Remaining repository artifact payload issues:")?;
        for issue in &outcome.after.issues {
            writeln!(
                &mut f,
                "  {}: {} ({})",
                repo_artifact_audit_issue_label(issue.kind),
                issue.path,
                issue.message
            )?;
        }
    }
    writeln!(&mut f, "Artifacts: {}", outcome.after.artifacts)?;
    writeln!(
        &mut f,
        "External payloads: {}",
        outcome.after.external_payloads
    )?;
    Ok(f)
}

fn format_large_file_fetch_outcome(outcome: &RepoLargeFileFetchOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if outcome.external_payloads == 0 {
        writeln!(
            &mut f,
            "No external payloads referenced by {}.",
            outcome.target
        )?;
    } else if outcome.fetched_payloads == 0 {
        writeln!(
            &mut f,
            "External payloads already present for {}.",
            outcome.target
        )?;
    } else {
        writeln!(
            &mut f,
            "Fetched {} external payload(s), {} byte(s), from {}.",
            outcome.fetched_payloads, outcome.fetched_bytes, outcome.remote
        )?;
    }
    writeln!(&mut f, "Remote: {}", outcome.remote)?;
    writeln!(&mut f, "Target: {}", outcome.target)?;
    writeln!(&mut f, "External payloads: {}", outcome.external_payloads)?;
    writeln!(
        &mut f,
        "Already present: {}",
        outcome.already_present_payloads
    )?;
    for file in &outcome.files {
        writeln!(
            &mut f,
            "  {} ({}, {} byte(s), {}, paths: {})",
            file.content_hash,
            large_file_fetch_status_label(file.status),
            file.size,
            file.store_path,
            file.paths.join(", ")
        )?;
    }
    Ok(f)
}

fn large_file_fetch_status_label(status: RepoLargeFileFetchStatus) -> &'static str {
    match status {
        RepoLargeFileFetchStatus::Present => "present",
        RepoLargeFileFetchStatus::Fetched => "fetched",
    }
}

fn format_large_file_status_outcome(
    outcome: &RepoLargeFileStatusOutcome,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if outcome.external_payloads == 0 {
        writeln!(
            &mut f,
            "No external payloads referenced by {}.",
            outcome.target
        )?;
    } else {
        writeln!(
            &mut f,
            "External payloads for {}: {} present, {} missing, {} invalid.",
            outcome.target,
            outcome.present_payloads,
            outcome.missing_payloads,
            outcome.invalid_payloads
        )?;
    }
    writeln!(&mut f, "External payloads: {}", outcome.external_payloads)?;
    writeln!(&mut f, "Present bytes: {}", outcome.present_bytes)?;
    writeln!(&mut f, "Missing bytes: {}", outcome.missing_bytes)?;
    writeln!(&mut f, "Invalid bytes: {}", outcome.invalid_bytes)?;
    for file in &outcome.files {
        let message = file
            .message
            .as_ref()
            .map(|message| format!(", {message}"))
            .unwrap_or_default();
        writeln!(
            &mut f,
            "  {} ({}, {} byte(s), {}, paths: {}{})",
            file.content_hash,
            large_file_status_state_label(file.status),
            file.size,
            file.store_path,
            file.paths.join(", "),
            message
        )?;
    }
    Ok(f)
}

fn large_file_status_state_label(status: RepoLargeFileStatusState) -> &'static str {
    match status {
        RepoLargeFileStatusState::Present => "present",
        RepoLargeFileStatusState::Missing => "missing",
        RepoLargeFileStatusState::Invalid => "invalid",
    }
}

fn format_large_file_prune_outcome(outcome: &RepoLargeFilePruneOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if outcome.candidate_payloads == 0 {
        writeln!(&mut f, "No unreferenced external payloads.")?;
    } else if outcome.dry_run {
        writeln!(
            &mut f,
            "Would prune {} external payload(s), {} byte(s).",
            outcome.candidate_payloads, outcome.candidate_bytes
        )?;
    } else {
        writeln!(
            &mut f,
            "Pruned {} external payload(s), {} byte(s).",
            outcome.pruned_payloads, outcome.pruned_bytes
        )?;
    }
    writeln!(
        &mut f,
        "Referenced external payloads: {}",
        outcome.referenced_payloads
    )?;
    for file in &outcome.files {
        writeln!(
            &mut f,
            "  {} ({} byte(s), {})",
            file.content_hash, file.size, file.path
        )?;
    }
    Ok(f)
}

fn repo_artifact_audit_issue_label(kind: RepoArtifactAuditIssueKind) -> &'static str {
    match kind {
        RepoArtifactAuditIssueKind::MissingObject => "missing object",
        RepoArtifactAuditIssueKind::InvalidObject => "invalid object",
        RepoArtifactAuditIssueKind::MissingExternalPayload => "missing external payload",
        RepoArtifactAuditIssueKind::InvalidExternalPayload => "invalid external payload",
    }
}

fn format_repo_tracked_paths(paths: &[RepoTrackedPath]) -> Result<String, ErrCtx> {
    format_repo_path_inventory(paths, "No tracked paths.")
}

fn format_repo_untracked_paths(paths: &[RepoTrackedPath]) -> Result<String, ErrCtx> {
    format_repo_path_inventory(paths, "No untracked paths.")
}

fn format_repo_path_inventory(
    paths: &[RepoTrackedPath],
    empty_message: &str,
) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if paths.is_empty() {
        writeln!(&mut f, "{empty_message}")?;
        return Ok(f);
    }
    for path in paths {
        match path.kind {
            RepoTrackedPathKind::SqliteDatabase => {
                if let Some(page_count) = path.page_count {
                    writeln!(
                        &mut f,
                        "{} (sqlite, {}, {page_count} page(s))",
                        path.path,
                        repo_path_storage_label(path.storage)
                    )?;
                } else {
                    writeln!(
                        &mut f,
                        "{} (sqlite, {}, {} byte(s))",
                        path.path,
                        repo_path_storage_label(path.storage),
                        path.size
                            .map(|size| size.to_string())
                            .unwrap_or_else(|| "?".to_string())
                    )?;
                }
            }
            RepoTrackedPathKind::TextFile | RepoTrackedPathKind::BinaryFile => writeln!(
                &mut f,
                "{} ({}, {}, {} byte(s))",
                path.path,
                repo_tracked_path_kind_label(path.kind),
                repo_path_storage_label(path.storage),
                path.size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )?,
        }
    }
    Ok(f)
}

fn format_repo_tracked_path_details(paths: &[RepoTrackedPathDetail]) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if paths.is_empty() {
        writeln!(&mut f, "No tracked paths.")?;
        return Ok(f);
    }
    for path in paths {
        match path.kind {
            RepoTrackedPathKind::SqliteDatabase => writeln!(
                &mut f,
                "{} (sqlite, {}, {} page(s))",
                path.path,
                repo_path_storage_label(path.storage),
                path.page_count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )?,
            RepoTrackedPathKind::TextFile | RepoTrackedPathKind::BinaryFile
                if path.storage == RepoPathStorage::External =>
            {
                writeln!(
                    &mut f,
                    "{} ({}, {}, {} byte(s), oid {}, hash {}, object {}, payload {})",
                    path.path,
                    repo_tracked_path_kind_label(path.kind),
                    repo_path_storage_label(path.storage),
                    path.size
                        .map(|size| size.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                    option_object_id_label(path.oid.as_ref()),
                    option_object_id_label(path.content_hash.as_ref()),
                    presence_label(path.object_present),
                    presence_label(path.external_payload_present)
                )?
            }
            RepoTrackedPathKind::TextFile | RepoTrackedPathKind::BinaryFile => writeln!(
                &mut f,
                "{} ({}, {}, {} byte(s), oid {}, hash {}, object {})",
                path.path,
                repo_tracked_path_kind_label(path.kind),
                repo_path_storage_label(path.storage),
                path.size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                option_object_id_label(path.oid.as_ref()),
                option_object_id_label(path.content_hash.as_ref()),
                presence_label(path.object_present)
            )?,
        }
    }
    Ok(f)
}

fn option_object_id_label(id: Option<&graft::repo::object::ObjectId>) -> String {
    id.map(ToString::to_string)
        .unwrap_or_else(|| "?".to_string())
}

fn presence_label(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "present",
        Some(false) => "missing",
        None => "n/a",
    }
}

fn format_repo_tracked_path_entries(paths: &[RepoTrackedPathEntry]) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if paths.is_empty() {
        writeln!(&mut f, "No tracked paths.")?;
        return Ok(f);
    }
    for path in paths {
        let mode = path
            .mode
            .map(|mode| mode.to_string())
            .unwrap_or_else(|| "------".to_string());
        let oid = path
            .oid
            .as_ref()
            .map(|oid| oid.short())
            .unwrap_or("------------");
        match path.kind {
            RepoTrackedPathKind::SqliteDatabase => writeln!(
                &mut f,
                "{} {mode} {oid} {} (sqlite, {}, {} page(s))",
                repo_index_stage_label(path.stage),
                path.path,
                repo_path_storage_label(path.storage),
                path.page_count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )?,
            RepoTrackedPathKind::TextFile | RepoTrackedPathKind::BinaryFile => writeln!(
                &mut f,
                "{} {mode} {oid} {} ({}, {}, {} byte(s))",
                repo_index_stage_label(path.stage),
                path.path,
                repo_tracked_path_kind_label(path.kind),
                repo_path_storage_label(path.storage),
                path.size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )?,
        }
    }
    Ok(f)
}

fn format_repo_config_entry(entry: &RepoConfigEntry) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(&mut f, "{} = {}", entry.key, entry.value)?;
    Ok(f)
}

fn format_repo_config_entries(entries: &[RepoConfigEntry]) -> Result<String, ErrCtx> {
    let mut f = String::new();
    for entry in entries {
        writeln!(&mut f, "{} = {}", entry.key, entry.value)?;
    }
    Ok(f)
}

fn repo_tracked_path_kind_label(kind: RepoTrackedPathKind) -> &'static str {
    match kind {
        RepoTrackedPathKind::SqliteDatabase => "sqlite",
        RepoTrackedPathKind::TextFile => "text file",
        RepoTrackedPathKind::BinaryFile => "binary file",
    }
}

fn repo_path_storage_label(storage: RepoPathStorage) -> &'static str {
    match storage {
        RepoPathStorage::SqliteSnapshot => "sqlite snapshot",
        RepoPathStorage::Inline => "inline",
        RepoPathStorage::External => "external",
    }
}

fn repo_index_stage_label(stage: graft::repo::index::IndexStage) -> &'static str {
    match stage {
        graft::repo::index::IndexStage::Normal => "normal",
        graft::repo::index::IndexStage::Base => "base",
        graft::repo::index::IndexStage::Ours => "ours",
        graft::repo::index::IndexStage::Theirs => "theirs",
    }
}

fn repo_tracked_path_kind_json_label(kind: RepoTrackedPathKind) -> &'static str {
    match kind {
        RepoTrackedPathKind::SqliteDatabase => "sqlite_database",
        RepoTrackedPathKind::TextFile => "text_file",
        RepoTrackedPathKind::BinaryFile => "binary_file",
    }
}

fn repo_path_storage_json_label(storage: RepoPathStorage) -> &'static str {
    match storage {
        RepoPathStorage::SqliteSnapshot => "sqlite_snapshot",
        RepoPathStorage::Inline => "inline",
        RepoPathStorage::External => "external",
    }
}

fn format_conflicts(status: &RepoStatus) -> Result<String, ErrCtx> {
    let mut f = String::new();
    if status.conflicted.is_empty() {
        writeln!(&mut f, "No conflicts.")?;
        return Ok(f);
    }

    writeln!(&mut f, "Unmerged paths:")?;
    for path in &status.conflicted {
        writeln!(&mut f, "  {path}")?;
    }
    writeln!(&mut f)?;
    writeln!(
        &mut f,
        "Resolve a path with `pragma graft_resolve = \"--ours [path]\"`, `pragma graft_resolve = \"--theirs [path]\"`, or `pragma graft_resolve = \"--manual [path]\"`."
    )?;
    Ok(f)
}

fn format_branches(
    branches: &[BranchInfo],
    remote_branches: &[RemoteBranchRef],
    mode: BranchListMode,
) -> Result<String, ErrCtx> {
    if branches.is_empty() && remote_branches.is_empty() && !matches!(mode, BranchListMode::Remote)
    {
        return Ok("No branches.".to_string());
    }
    if remote_branches.is_empty() && matches!(mode, BranchListMode::Remote) {
        return Ok("No remote branches.".to_string());
    }

    let mut f = String::new();
    if !matches!(mode, BranchListMode::Remote) {
        for branch in branches {
            let marker = if branch.current { "*" } else { " " };
            let target = branch
                .target
                .as_deref()
                .map_or("(unborn)", |target| &target[..target.len().min(12)]);
            let upstream = branch
                .upstream
                .as_ref()
                .map(|upstream| format!(" [{}{}{}]", upstream.remote, "/", upstream.branch))
                .unwrap_or_default();
            writeln!(&mut f, "{marker} {:<24} {target}{upstream}", branch.name)?;
        }
    }
    if mode.includes_remote() {
        for branch in remote_branches {
            let name = if matches!(mode, BranchListMode::All) {
                format!("remotes/{}/{}", branch.remote, branch.branch)
            } else {
                format!("{}/{}", branch.remote, branch.branch)
            };
            writeln!(
                &mut f,
                "  {name:<24} {}",
                &branch.head[..branch.head.len().min(12)]
            )?;
        }
    }
    Ok(f)
}

fn format_branch_created(branch: &BranchInfo) -> String {
    match &branch.target {
        Some(target) => format!("Created branch '{}' at {}", branch.name, &target[..12]),
        None => format!("Created unborn branch '{}'", branch.name),
    }
}

fn format_branch_upstream(branch: &BranchInfo) -> String {
    match &branch.upstream {
        Some(upstream) => format!(
            "Branch '{}' set to track {}/{}",
            branch.name, upstream.remote, upstream.branch
        ),
        None => format!("Branch '{}' has no upstream", branch.name),
    }
}

fn format_branch_upstream_unset(branch: &BranchInfo) -> String {
    format!("Branch '{}' upstream unset", branch.name)
}

fn format_branch_deleted(branch: &BranchInfo, force: bool) -> String {
    let forced = if force { " forcibly" } else { "" };
    match &branch.target {
        Some(target) => format!(
            "Deleted branch '{}'{} (was {})",
            branch.name,
            forced,
            &target[..target.len().min(12)]
        ),
        None => format!("Deleted unborn branch '{}'{}", branch.name, forced),
    }
}

fn format_branch_renamed(old: &str, branch: &BranchInfo, force: bool) -> String {
    let forced = if force { " forcibly" } else { "" };
    match &branch.target {
        Some(target) => format!(
            "Renamed branch '{}' to '{}'{} at {}",
            old,
            branch.name,
            forced,
            &target[..target.len().min(12)]
        ),
        None => format!(
            "Renamed unborn branch '{}' to '{}'{}",
            old, branch.name, forced
        ),
    }
}

fn format_repo_tags(tags: &[TagInfo]) -> Result<String, ErrCtx> {
    if tags.is_empty() {
        return Ok("No tags.".to_string());
    }

    let mut f = String::new();
    for tag in tags {
        writeln!(
            &mut f,
            "{:<24} {}{}",
            tag.name,
            &tag.target[..tag.target.len().min(12)],
            if tag.annotated {
                format!(" (annotated {})", &tag.object[..tag.object.len().min(12)])
            } else {
                String::new()
            }
        )?;
    }
    Ok(f)
}

fn format_tag_created(tag: &TagInfo) -> String {
    if tag.annotated {
        format!(
            "Created annotated tag '{}' at {} ({})",
            tag.name,
            &tag.target[..tag.target.len().min(12)],
            &tag.object[..tag.object.len().min(12)]
        )
    } else {
        format!(
            "Created tag '{}' at {}",
            tag.name,
            &tag.target[..tag.target.len().min(12)]
        )
    }
}

fn format_tag_deleted(tag: &TagInfo) -> String {
    if tag.annotated {
        format!(
            "Deleted annotated tag '{}' (was {} via {})",
            tag.name,
            &tag.target[..tag.target.len().min(12)],
            &tag.object[..tag.object.len().min(12)]
        )
    } else {
        format!(
            "Deleted tag '{}' (was {})",
            tag.name,
            &tag.target[..tag.target.len().min(12)]
        )
    }
}

fn format_merge_outcome(outcome: &MergeOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    match outcome {
        MergeOutcome::FastForward { from, to } => {
            if let Some(from) = from {
                writeln!(
                    &mut f,
                    "Fast-forward {}..{}",
                    &from[..from.len().min(12)],
                    &to[..to.len().min(12)]
                )?;
            } else {
                writeln!(&mut f, "Fast-forward to {}", &to[..to.len().min(12)])?;
            }
        }
        MergeOutcome::AlreadyUpToDate { head } => {
            writeln!(
                &mut f,
                "Already up to date at {}",
                &head[..head.len().min(12)]
            )?;
        }
        MergeOutcome::Merged {
            target, merge_base, staged, conflicted, ..
        } => {
            writeln!(&mut f, "Merged {}", &target[..target.len().min(12)])?;
            if let Some(merge_base) = merge_base {
                writeln!(
                    &mut f,
                    "Merge base {}",
                    &merge_base[..merge_base.len().min(12)]
                )?;
            }
            if !staged.is_empty() {
                writeln!(&mut f, "Staged paths:")?;
                for path in staged {
                    writeln!(&mut f, "  {path}")?;
                }
            }
            if !conflicted.is_empty() {
                writeln!(&mut f, "Unmerged paths:")?;
                for path in conflicted {
                    writeln!(&mut f, "  {path}")?;
                }
            }
            if staged.is_empty() && conflicted.is_empty() {
                writeln!(&mut f, "No changes.")?;
            }
        }
    }
    Ok(f)
}

fn format_merge_outcome_with_row_auto_merge(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    outcome: &MergeOutcome,
    row_auto_merge: Option<&RowAutoMergeResult>,
    remote: Option<Arc<Remote>>,
) -> Result<String, ErrCtx> {
    let display_outcome = row_auto_merge
        .map(|result| merge_outcome_with_row_auto_merge(outcome, &result.key))
        .unwrap_or_else(|| outcome.clone());
    let mut f = format_merge_outcome(&display_outcome)?;
    if let Some(result) = row_auto_merge {
        append_row_auto_merge_result(&mut f, result)?;
    } else {
        append_row_merge_analysis(&mut f, runtime, file, repo, outcome, remote)?;
    }
    Ok(f)
}

fn merge_outcome_with_row_auto_merge(outcome: &MergeOutcome, key: &str) -> MergeOutcome {
    let MergeOutcome::Merged {
        head,
        target,
        merge_base,
        staged,
        conflicted,
    } = outcome
    else {
        return outcome.clone();
    };

    let mut staged = staged.clone();
    if !staged.iter().any(|path| path == key) {
        staged.push(key.to_string());
        staged.sort();
    }
    let conflicted = conflicted
        .iter()
        .filter(|path| path.as_str() != key)
        .cloned()
        .collect();

    MergeOutcome::Merged {
        head: head.clone(),
        target: target.clone(),
        merge_base: merge_base.clone(),
        staged,
        conflicted,
    }
}

fn append_row_auto_merge_result(
    output: &mut String,
    result: &RowAutoMergeResult,
) -> Result<(), ErrCtx> {
    if !output.ends_with('\n') {
        output.push('\n');
    }
    writeln!(output, "Row-level auto-merged {}:", result.key)?;
    writeln!(
        output,
        "  applied {} row change(s) from theirs",
        result.applied_changes
    )?;
    writeln!(output, "  ours: {} row change(s)", result.ours_changes)?;
    writeln!(output, "  theirs: {} row change(s)", result.theirs_changes)?;
    Ok(())
}

fn format_pull_outcome(outcome: &PullOutcome) -> Result<String, ErrCtx> {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Fetched {}/{} at {} ({} new commits)",
        outcome.remote,
        outcome.remote_branch,
        &outcome.head[..outcome.head.len().min(12)],
        outcome.commits
    )?;
    match &outcome.merge {
        MergeOutcome::FastForward { from, to } => {
            if let Some(from) = from {
                writeln!(
                    &mut f,
                    "Fast-forwarded {} {}..{}",
                    outcome.local_branch,
                    &from[..from.len().min(12)],
                    &to[..to.len().min(12)]
                )?;
            } else {
                writeln!(
                    &mut f,
                    "Fast-forwarded {} to {}",
                    outcome.local_branch,
                    &to[..to.len().min(12)]
                )?;
            }
        }
        MergeOutcome::AlreadyUpToDate { head } => {
            writeln!(
                &mut f,
                "{} already up to date at {}",
                outcome.local_branch,
                &head[..head.len().min(12)]
            )?;
        }
        MergeOutcome::Merged {
            target, merge_base, staged, conflicted, ..
        } => {
            writeln!(
                &mut f,
                "Merged {}/{} ({}) into {}",
                outcome.remote,
                outcome.remote_branch,
                &target[..target.len().min(12)],
                outcome.local_branch
            )?;
            if let Some(merge_base) = merge_base {
                writeln!(
                    &mut f,
                    "Merge base {}",
                    &merge_base[..merge_base.len().min(12)]
                )?;
            }
            if !staged.is_empty() {
                writeln!(&mut f, "Staged paths:")?;
                for path in staged {
                    writeln!(&mut f, "  {path}")?;
                }
            }
            if !conflicted.is_empty() {
                writeln!(&mut f, "Unmerged paths:")?;
                for path in conflicted {
                    writeln!(&mut f, "  {path}")?;
                }
            }
            if conflicted.is_empty() {
                writeln!(&mut f, "Commit to complete the merge.")?;
            }
        }
    }
    Ok(f)
}

fn format_pull_outcome_with_row_analysis(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    outcome: &PullOutcome,
    remote: Option<Arc<Remote>>,
) -> Result<String, ErrCtx> {
    let mut f = format_pull_outcome(outcome)?;
    append_row_merge_analysis(&mut f, runtime, file, repo, &outcome.merge, remote)?;
    Ok(f)
}

fn append_row_merge_analysis(
    output: &mut String,
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    outcome: &MergeOutcome,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    let MergeOutcome::Merged { conflicted, .. } = outcome else {
        return Ok(());
    };
    let key = repo.file_key(&file.tag)?;
    if !conflicted.iter().any(|path| path == &key) {
        return Ok(());
    }

    if !output.ends_with('\n') {
        output.push('\n');
    }
    match format_current_file_row_merge_analysis(runtime, repo, &key, remote) {
        Ok(Some(analysis)) => output.push_str(&analysis),
        Ok(None) => {}
        Err(err) => {
            writeln!(output, "Row-level analysis for {key} unavailable: {err}")?;
        }
    }
    Ok(())
}

fn format_current_file_row_merge_analysis(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    remote: Option<Arc<Remote>>,
) -> Result<Option<String>, ErrCtx> {
    let index = repo.read_index()?;
    let mut base = None;
    let mut ours = None;
    let mut theirs = None;

    for entry in index.entries.iter().filter(|entry| entry.path == key) {
        match entry.stage {
            graft::repo::index::IndexStage::Base => base = entry.file.as_ref(),
            graft::repo::index::IndexStage::Ours => ours = entry.file.as_ref(),
            graft::repo::index::IndexStage::Theirs => theirs = entry.file.as_ref(),
            graft::repo::index::IndexStage::Normal => {}
        }
    }

    let (Some(base), Some(ours), Some(theirs)) = (base, ours, theirs) else {
        return Ok(Some(formatdoc!(
            "
            Row-level analysis for {key}:
              unavailable: merge involves add/delete of this tracked path.
            "
        )));
    };

    hydrate_repo_file_state_for(runtime, base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, theirs, remote, RepoSnapshotPurpose::Merge)?;
    let plan = plan_repo_snapshot_merge(runtime, repo, base, ours, theirs)?;
    let analysis = &plan.analysis;
    let mut f = String::new();
    writeln!(&mut f, "Row-level analysis for {key}:")?;
    writeln!(&mut f, "  ours: {} row change(s)", analysis.ours_changes)?;
    writeln!(
        &mut f,
        "  theirs: {} row change(s)",
        analysis.theirs_changes
    )?;
    if !plan.resolved_opaque_changes().is_empty() {
        writeln!(
            &mut f,
            "  resolved opaque change(s): {}",
            plan.resolved_opaque_changes().len()
        )?;
    }
    if plan.has_opaque_changes() {
        writeln!(
            &mut f,
            "  unresolved opaque change(s): {}",
            plan.opaque_changes()
        )?;
    }
    if !plan.schema_conflicts().is_empty() {
        writeln!(
            &mut f,
            "  schema conflict(s): {}",
            plan.schema_conflicts().len()
        )?;
    }
    if analysis.has_conflicts() {
        writeln!(&mut f, "  Row conflicts:")?;
        for conflict in &analysis.conflicts {
            writeln!(
                &mut f,
                "    {} rowid={} (ours {}, theirs {})",
                conflict.table,
                conflict.rowid,
                row_change_kind_label(conflict.ours),
                row_change_kind_label(conflict.theirs)
            )?;
        }
    } else if !plan.has_opaque_changes() && plan.schema_conflicts().is_empty() {
        writeln!(
            &mut f,
            "  No row conflicts detected; row-level auto-merge candidate."
        )?;
    } else {
        writeln!(&mut f, "  No row conflicts detected.")?;
    }
    Ok(Some(f))
}

fn current_file_status_row_merge_analysis(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<Option<JsonRowMergeAnalysis>, ErrCtx> {
    let key = repo.file_key(&file.tag)?;
    current_file_row_merge_analysis(runtime, repo, &key, remote)
}

fn current_file_status_row_merge_analysis_lossy(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Option<JsonRowMergeAnalysis> {
    match current_file_status_row_merge_analysis(runtime, file, repo, remote) {
        Ok(analysis) => analysis,
        Err(err) => {
            let path = repo
                .file_key(&file.tag)
                .unwrap_or_else(|_| "db.sqlite3".to_string());
            Some(JsonRowMergeAnalysis {
                path,
                available: false,
                can_auto_merge: false,
                ours_changes: 0,
                theirs_changes: 0,
                apply_changes: 0,
                opaque_changes: 0,
                resolved_opaque_changes: 0,
                resolved_opaque_change_details: vec![],
                apply_policy: row_merge_apply_policy(&crate::row_merge::RowMergePolicy::default()),
                limitations: vec![],
                blocked_reasons: vec!["analysis_error"],
                row_conflicts: vec![],
                schema_conflicts: vec![],
                message: Some(format!("row-level analysis unavailable: {err}")),
            })
        }
    }
}

fn current_file_row_merge_analysis(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    remote: Option<Arc<Remote>>,
) -> Result<Option<JsonRowMergeAnalysis>, ErrCtx> {
    let index = repo.read_index()?;
    if !index.conflicted_paths().iter().any(|path| path == key) {
        return Ok(None);
    }

    let Some((base, ours, theirs)) = current_file_conflict_states(repo, key)? else {
        return Ok(Some(JsonRowMergeAnalysis {
            path: key.to_string(),
            available: false,
            can_auto_merge: false,
            ours_changes: 0,
            theirs_changes: 0,
            apply_changes: 0,
            opaque_changes: 0,
            resolved_opaque_changes: 0,
            resolved_opaque_change_details: vec![],
            apply_policy: row_merge_apply_policy(&crate::row_merge::RowMergePolicy::default()),
            limitations: vec![],
            blocked_reasons: vec!["add_delete_conflict"],
            row_conflicts: vec![],
            schema_conflicts: vec![],
            message: Some("merge involves add/delete of this tracked path".to_string()),
        }));
    };

    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
    let row_conflicts: Vec<JsonRowMergeConflict> = plan
        .analysis
        .conflicts
        .iter()
        .map(|conflict| JsonRowMergeConflict {
            reason: conflict.reason.as_str(),
            table: conflict.table.clone(),
            columns: conflict.columns.clone(),
            rowid: conflict.rowid,
            ours_rowid: (conflict.ours_rowid != conflict.rowid).then_some(conflict.ours_rowid),
            theirs_rowid: (conflict.theirs_rowid != conflict.rowid)
                .then_some(conflict.theirs_rowid),
            semantic_key: conflict.semantic_key.clone(),
            ours: row_change_kind_label(conflict.ours),
            theirs: row_change_kind_label(conflict.theirs),
            base_row: json_record_values_opt(conflict.base_row.as_ref()),
            ours_row: json_record_values_opt(conflict.ours_row.as_ref()),
            theirs_row: json_record_values_opt(conflict.theirs_row.as_ref()),
        })
        .collect();
    let schema_conflicts: Vec<JsonSchemaMergeConflict> = plan
        .schema_conflicts()
        .iter()
        .map(|conflict| JsonSchemaMergeConflict {
            reason: conflict.reason.as_str(),
            name: conflict.name.clone(),
            entry_type: conflict.entry_type.clone(),
            ours: conflict.ours.map(schema_change_kind_label),
            theirs: conflict.theirs.map(schema_change_kind_label),
            column_changes: json_schema_column_changes(&conflict.column_changes),
            message: conflict.message,
        })
        .collect();
    let apply_changes = plan.apply_change_count();
    let mut blocked_reasons = Vec::new();
    if !row_conflicts.is_empty() {
        blocked_reasons.push("row_conflicts");
    }
    if !schema_conflicts.is_empty() {
        blocked_reasons.push("schema_conflicts");
    }
    if plan.opaque_changes() > 0 {
        blocked_reasons.push("opaque_changes");
    }
    if apply_changes == 0 {
        blocked_reasons.push("no_applicable_changes");
    }
    let can_auto_merge = blocked_reasons.is_empty();

    Ok(Some(JsonRowMergeAnalysis {
        path: key.to_string(),
        available: true,
        can_auto_merge,
        ours_changes: plan.analysis.ours_changes,
        theirs_changes: plan.analysis.theirs_changes,
        apply_changes,
        opaque_changes: plan.opaque_changes(),
        resolved_opaque_changes: plan.resolved_opaque_changes().len(),
        resolved_opaque_change_details: json_resolved_opaque_changes(
            plan.resolved_opaque_changes(),
        ),
        apply_policy: row_merge_apply_policy(plan.policy()),
        limitations: json_limitations(&plan.limitations()),
        blocked_reasons,
        row_conflicts,
        schema_conflicts,
        message: None,
    }))
}

fn row_merge_apply_policy(policy: &crate::row_merge::RowMergePolicy) -> JsonRowMergeApplyPolicy {
    JsonRowMergeApplyPolicy {
        foreign_keys: "disabled_during_apply_checked_after",
        triggers: "disabled_during_apply",
        validation: vec!["integrity_check", "foreign_key_check"],
        default_semantic_keys: policy.default_semantic_keys.clone(),
        internal_resolvers: json_internal_resolvers(policy),
        schema_resolvers: policy
            .schema_resolvers
            .iter()
            .map(|(operation, resolver)| JsonRowMergeSchemaResolver {
                operation: operation.clone(),
                resolver: resolver.as_str(),
            })
            .collect(),
        generated_columns: policy
            .generated_columns
            .iter()
            .map(|(table, columns)| JsonRowMergeGeneratedColumns {
                table: table.clone(),
                columns: columns.clone(),
            })
            .collect(),
    }
}

fn json_internal_resolvers(
    policy: &crate::row_merge::RowMergePolicy,
) -> Vec<JsonRowMergeInternalResolver> {
    policy
        .internal_resolvers
        .iter()
        .map(|(table, resolver)| JsonRowMergeInternalResolver {
            table: table.clone(),
            resolver: resolver.as_str(),
        })
        .collect()
}

fn repo_conflict_artifacts(
    runtime: &Runtime,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<JsonConflictList, ErrCtx> {
    let status = repo.status()?;
    let resolution_state = read_row_conflict_resolution_state(repo, status.merge_head.as_deref())?;
    let mut conflicts = Vec::new();
    for path in &status.conflicted {
        conflicts.extend(repo_path_conflict_artifacts(
            runtime,
            repo,
            path,
            remote.clone(),
            &resolution_state,
        )?);
    }
    let paths = json_conflict_paths(&conflicts);
    let current_head = status.head_target.clone();
    let current_branch = repo.current_branch()?;
    Ok(JsonConflictList {
        current_head,
        current_branch,
        merge_head: status.merge_head,
        paths,
        conflicts,
    })
}

fn json_conflict_paths(conflicts: &[JsonConflictArtifact]) -> Vec<JsonConflictPath> {
    #[derive(Clone, Copy)]
    struct Counts {
        kind: &'static str,
        storage: &'static str,
        total: usize,
        unresolved: usize,
        resolved: usize,
    }

    let mut by_path = BTreeMap::<String, Counts>::new();
    for conflict in conflicts {
        let entry = by_path.entry(conflict.path.clone()).or_insert(Counts {
            kind: conflict.path_kind,
            storage: conflict.storage,
            total: 0,
            unresolved: 0,
            resolved: 0,
        });
        entry.kind = conflict.path_kind;
        entry.storage = conflict.storage;
        entry.total += 1;
        if conflict.status == "resolved" {
            entry.resolved += 1;
        } else {
            entry.unresolved += 1;
        }
    }

    by_path
        .into_iter()
        .map(|(path, counts)| JsonConflictPath {
            path,
            kind: counts.kind,
            storage: counts.storage,
            status: if counts.unresolved == 0 {
                "resolved"
            } else {
                "unresolved"
            },
            total: counts.total,
            unresolved: counts.unresolved,
            resolved: counts.resolved,
        })
        .collect()
}

fn unresolved_conflict_artifact_count(
    runtime: &Runtime,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<usize, ErrCtx> {
    Ok(repo_conflict_artifacts(runtime, repo, remote)?
        .conflicts
        .iter()
        .filter(|conflict| conflict.status == "unresolved")
        .count())
}

fn conflict_path_kind(repo: &Repository, key: &str) -> Result<RepoTrackedPathKind, ErrCtx> {
    conflict_path_descriptor(repo, key).map(|(kind, _)| kind)
}

fn conflict_path_storage(repo: &Repository, key: &str) -> Result<RepoPathStorage, ErrCtx> {
    conflict_path_descriptor(repo, key).map(|(_, storage)| storage)
}

fn conflict_path_descriptor(
    repo: &Repository,
    key: &str,
) -> Result<(RepoTrackedPathKind, RepoPathStorage), ErrCtx> {
    let index = repo.read_index()?;
    for entry in index.entries.iter().filter(|entry| entry.path == key) {
        if entry.file.is_some() {
            return Ok((
                RepoTrackedPathKind::SqliteDatabase,
                RepoPathStorage::SqliteSnapshot,
            ));
        }
        if let Some(artifact) = &entry.artifact {
            return Ok((
                artifact_checkout_path_kind(artifact),
                artifact_checkout_path_storage(artifact),
            ));
        }
    }
    Ok((RepoTrackedPathKind::BinaryFile, RepoPathStorage::Inline))
}

fn repo_path_conflict_artifacts(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    remote: Option<Arc<Remote>>,
    resolution_state: &RowConflictResolutionState,
) -> Result<Vec<JsonConflictArtifact>, ErrCtx> {
    let (path_kind, path_storage) = conflict_path_descriptor(repo, key)?;
    let path_kind_label = repo_tracked_path_kind_json_label(path_kind);
    let path_storage_label = repo_path_storage_json_label(path_storage);
    let Some((base, ours, theirs)) = current_file_conflict_states(repo, key)? else {
        return Ok(vec![file_conflict_artifact(
            key,
            path_kind_label,
            path_storage_label,
            "file",
            "add_delete_conflict",
            Some("merge involves add/delete of this tracked path".to_string()),
        )]);
    };

    let result = (|| {
        hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
        hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
        hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;
        let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
        let mut artifacts = Vec::new();

        for conflict in &plan.analysis.conflicts {
            let resolution = resolution_state
                .rows
                .get(&row_conflict_resolution_key(
                    key,
                    &conflict.table,
                    conflict.rowid,
                ))
                .and_then(|label| match label.as_str() {
                    "ours" => Some("ours"),
                    "theirs" => Some("theirs"),
                    _ => None,
                });
            artifacts.push(JsonConflictArtifact {
                id: format!("{}:row:{}:{}", key, conflict.table, conflict.rowid),
                path: key.to_string(),
                path_kind: "sqlite_database",
                storage: path_storage_label,
                kind: "row",
                reason: conflict.reason.as_str(),
                status: if resolution.is_some() {
                    "resolved"
                } else {
                    "unresolved"
                },
                resolution,
                table: Some(conflict.table.clone()),
                columns: Some(conflict.columns.clone()).filter(|columns| !columns.is_empty()),
                rowid: Some(conflict.rowid),
                ours_rowid: (conflict.ours_rowid != conflict.rowid).then_some(conflict.ours_rowid),
                theirs_rowid: (conflict.theirs_rowid != conflict.rowid)
                    .then_some(conflict.theirs_rowid),
                semantic_key: conflict.semantic_key.clone(),
                name: None,
                entry_type: None,
                column_changes: Vec::new(),
                change: None,
                owner: None,
                ours_op: Some(row_change_kind_label(conflict.ours)),
                theirs_op: Some(row_change_kind_label(conflict.theirs)),
                base_row: json_record_values_opt(conflict.base_row.as_ref()),
                ours_row: json_record_values_opt(conflict.ours_row.as_ref()),
                theirs_row: json_record_values_opt(conflict.theirs_row.as_ref()),
                message: None,
            });
        }

        for conflict in plan.schema_conflicts() {
            artifacts.push(JsonConflictArtifact {
                id: format!("{}:schema:{}:{}", key, conflict.entry_type, conflict.name),
                path: key.to_string(),
                path_kind: "sqlite_database",
                storage: path_storage_label,
                kind: "schema",
                reason: conflict.reason.as_str(),
                status: "unresolved",
                resolution: None,
                table: None,
                columns: None,
                rowid: None,
                ours_rowid: None,
                theirs_rowid: None,
                semantic_key: None,
                name: Some(conflict.name.clone()),
                entry_type: Some(conflict.entry_type.clone()),
                column_changes: json_schema_column_changes(&conflict.column_changes),
                change: None,
                owner: None,
                ours_op: conflict.ours.map(schema_change_kind_label),
                theirs_op: conflict.theirs.map(schema_change_kind_label),
                base_row: None,
                ours_row: None,
                theirs_row: None,
                message: Some(conflict.message.to_string()),
            });
        }

        for change in plan.unresolved_opaque_changes() {
            artifacts.push(JsonConflictArtifact {
                id: format!("{}:opaque:{}:{}", key, change.reason.as_str(), change.name),
                path: key.to_string(),
                path_kind: "sqlite_database",
                storage: path_storage_label,
                kind: "opaque",
                reason: change.reason.as_str(),
                status: "unresolved",
                resolution: None,
                table: None,
                columns: None,
                rowid: None,
                ours_rowid: None,
                theirs_rowid: None,
                semantic_key: None,
                name: Some(change.name.clone()),
                entry_type: None,
                column_changes: Vec::new(),
                change: Some(change.change.as_str()),
                owner: change.owner.clone(),
                ours_op: None,
                theirs_op: None,
                base_row: None,
                ours_row: None,
                theirs_row: None,
                message: Some(opaque_conflict_message(change).to_string()),
            });
        }

        if artifacts.is_empty() && plan.apply_change_count() == 0 {
            artifacts.push(file_conflict_artifact(
                key,
                path_kind_label,
                path_storage_label,
                "file",
                "no_applicable_changes",
                Some("no row or schema conflict details were produced".to_string()),
            ));
        }

        Ok::<_, ErrCtx>(artifacts)
    })();

    match result {
        Ok(artifacts) => Ok(artifacts),
        Err(err) => Ok(vec![file_conflict_artifact(
            key,
            path_kind_label,
            path_storage_label,
            "file",
            "analysis_error",
            Some(format!("row-level conflict analysis unavailable: {err}")),
        )]),
    }
}

fn file_conflict_artifact(
    key: &str,
    path_kind: &'static str,
    path_storage: &'static str,
    kind: &'static str,
    reason: &'static str,
    message: Option<String>,
) -> JsonConflictArtifact {
    JsonConflictArtifact {
        id: format!("{key}:{kind}:{reason}"),
        path: key.to_string(),
        path_kind,
        storage: path_storage,
        kind,
        reason,
        status: "unresolved",
        resolution: None,
        table: None,
        columns: None,
        rowid: None,
        ours_rowid: None,
        theirs_rowid: None,
        semantic_key: None,
        name: None,
        entry_type: None,
        column_changes: Vec::new(),
        change: None,
        owner: None,
        ours_op: None,
        theirs_op: None,
        base_row: None,
        ours_row: None,
        theirs_row: None,
        message,
    }
}

fn json_record_values_opt(
    record: Option<&crate::sqlite_parse::Record>,
) -> Option<Vec<serde_json::Value>> {
    record.map(|record| {
        record
            .values
            .iter()
            .map(crate::json::JsonRowChange::value_to_json)
            .collect()
    })
}

fn json_schema_column_changes(
    changes: &[crate::row_merge::SchemaMergeColumnChange],
) -> Vec<JsonSchemaColumnChange> {
    changes
        .iter()
        .map(|change| JsonSchemaColumnChange {
            side: change.side.as_str(),
            operation: change.operation.as_str(),
            from: change.from.clone(),
            to: change.to.clone(),
        })
        .collect()
}

fn json_resolved_opaque_changes(
    changes: &[crate::row_merge::RowMergeResolvedOpaqueChange],
) -> Vec<JsonResolvedOpaqueChange> {
    changes
        .iter()
        .map(|change| JsonResolvedOpaqueChange {
            name: change.name.clone(),
            reason: change.reason.as_str(),
            resolver: change.resolver.as_str(),
        })
        .collect()
}

fn opaque_conflict_message(change: &crate::row_level_diff::OpaqueChange) -> &'static str {
    match change.reason {
        crate::row_level_diff::OpaqueChangeReason::VirtualTable => {
            "virtual table changes require application-specific resolution"
        }
        crate::row_level_diff::OpaqueChangeReason::FtsShadowTable => {
            "FTS shadow table changes must be rebuilt or resolved with their owner table"
        }
        crate::row_level_diff::OpaqueChangeReason::WithoutRowidTable => {
            "WITHOUT ROWID table changes are outside row-level merge support"
        }
        crate::row_level_diff::OpaqueChangeReason::SqliteInternalTable => {
            "SQLite internal table changes require an explicit resolver policy"
        }
        crate::row_level_diff::OpaqueChangeReason::IndexBtree => {
            "SQLite index B-tree changes require an explicit resolver policy"
        }
    }
}

fn row_change_kind_label(kind: crate::row_merge::RowChangeKind) -> &'static str {
    match kind {
        crate::row_merge::RowChangeKind::Insert => "insert",
        crate::row_merge::RowChangeKind::Delete => "delete",
        crate::row_merge::RowChangeKind::Update => "update",
    }
}

fn schema_change_kind_label(kind: crate::row_level_diff::SchemaChangeKind) -> &'static str {
    match kind {
        crate::row_level_diff::SchemaChangeKind::Added => "added",
        crate::row_level_diff::SchemaChangeKind::Deleted => "deleted",
        crate::row_level_diff::SchemaChangeKind::Modified => "modified",
    }
}

#[derive(Debug)]
struct RowAutoMergeResult {
    key: String,
    applied_changes: usize,
    ours_changes: usize,
    theirs_changes: usize,
}

fn try_row_auto_merge_current_file_conflict(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    outcome: &MergeOutcome,
    remote: Option<Arc<Remote>>,
) -> Result<Option<RowAutoMergeResult>, ErrCtx> {
    let MergeOutcome::Merged { conflicted, .. } = outcome else {
        return Ok(None);
    };
    let key = repo.file_key(&file.tag)?;
    if !conflicted.iter().any(|path| path == &key) {
        return Ok(None);
    }

    try_row_merge_current_file_status_conflict(runtime, file, repo, remote, true)
}

fn try_row_auto_merge_current_file_status_conflict(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
) -> Result<Option<RowAutoMergeResult>, ErrCtx> {
    try_row_merge_current_file_status_conflict(runtime, file, repo, remote, false)
}

fn try_row_merge_current_file_status_conflict(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
    allow_partial: bool,
) -> Result<Option<RowAutoMergeResult>, ErrCtx> {
    let key = repo.file_key(&file.tag)?;
    let index = repo.read_index()?;
    if !index.conflicted_paths().iter().any(|path| path == &key) {
        return Ok(None);
    }

    let Some((base, ours, theirs)) = current_file_conflict_states(repo, &key)? else {
        return Ok(None);
    };

    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = plan_repo_snapshot_merge(runtime, repo, &base, &ours, &theirs)?;
    if plan.has_opaque_changes()
        || !plan.schema_conflicts().is_empty()
        || plan.apply_change_count() == 0
    {
        return Ok(None);
    }
    if plan.analysis.has_conflicts() && !allow_partial {
        return Ok(None);
    }

    let applied_changes = plan.apply_change_count();
    let sql = plan.theirs_apply_sql();
    let merged = materialize_row_auto_merge_state(runtime, repo, &key, &ours, &sql)?;
    checkout_repo_file_state(runtime, file, &merged, None)?;
    if plan.analysis.has_conflicts() {
        return Ok(None);
    }
    repo.resolve_file_conflict(&file.tag, Some(merged))?;

    Ok(Some(RowAutoMergeResult {
        key,
        applied_changes,
        ours_changes: plan.analysis.ours_changes,
        theirs_changes: plan.analysis.theirs_changes,
    }))
}

fn current_file_conflict_states(
    repo: &Repository,
    key: &str,
) -> Result<Option<(CommitFileState, CommitFileState, CommitFileState)>, ErrCtx> {
    let index = repo.read_index()?;
    let mut base = None;
    let mut ours = None;
    let mut theirs = None;

    for entry in index.entries.iter().filter(|entry| entry.path == key) {
        match entry.stage {
            graft::repo::index::IndexStage::Base => base = entry.file.clone(),
            graft::repo::index::IndexStage::Ours => ours = entry.file.clone(),
            graft::repo::index::IndexStage::Theirs => theirs = entry.file.clone(),
            graft::repo::index::IndexStage::Normal => {}
        }
    }

    Ok(match (base, ours, theirs) {
        (Some(base), Some(ours), Some(theirs)) => Some((base, ours, theirs)),
        _ => None,
    })
}

fn materialize_row_auto_merge_state(
    runtime: &Runtime,
    repo: &Repository,
    key: &str,
    ours: &CommitFileState,
    sql: &str,
) -> Result<CommitFileState, ErrCtx> {
    let temp_path = row_auto_merge_temp_path(repo, key)?;
    let result = (|| {
        write_repo_file_state_to_path(runtime, ours, &temp_path)?;
        apply_row_merge_sql_to_path(&temp_path, sql)?;
        import_physical_sqlite_file_state(runtime, &temp_path)
    })();
    let cleanup = std::fs::remove_file(&temp_path);
    match (result, cleanup) {
        (Ok(state), Ok(()) | Err(_)) => Ok(state),
        (Err(err), Ok(()) | Err(_)) => Err(err),
    }
}

fn row_auto_merge_temp_path(repo: &Repository, key: &str) -> Result<PathBuf, ErrCtx> {
    let dir = repo.worktree().join(".graft").join("tmp");
    std::fs::create_dir_all(&dir)?;
    let id = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
    let key = key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    Ok(dir.join(format!("row-merge-{}-{id}-{key}.db", std::process::id())))
}

fn apply_row_merge_sql_to_path(path: &Path, sql: &str) -> Result<(), ErrCtx> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| row_auto_merge_sqlite_err(path, "open temporary database", err))?;
    conn.execute_batch("PRAGMA foreign_keys = OFF;")
        .map_err(|err| row_auto_merge_sqlite_err(path, "disable foreign keys", err))?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER, false)
        .map_err(|err| row_auto_merge_sqlite_err(path, "disable triggers", err))?;
    conn.execute_batch(sql)
        .map_err(|err| row_auto_merge_sqlite_err(path, "apply row changes", err))?;
    validate_row_merge_sqlite(path, &conn)?;
    Ok(())
}

fn validate_row_merge_sqlite(path: &Path, conn: &rusqlite::Connection) -> Result<(), ErrCtx> {
    let mut integrity_stmt = conn
        .prepare("PRAGMA integrity_check;")
        .map_err(|err| row_auto_merge_sqlite_err(path, "prepare integrity_check", err))?;
    let integrity_rows = integrity_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|err| row_auto_merge_sqlite_err(path, "run integrity_check", err))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|err| row_auto_merge_sqlite_err(path, "read integrity_check", err))?;
    if integrity_rows.is_empty() || integrity_rows.iter().any(|row| row != "ok") {
        return Err(ErrCtx::PragmaErr(
            format!(
                "row-level auto-merge failed integrity_check at `{}`: {}",
                path.display(),
                integrity_rows.join("; ")
            )
            .into(),
        ));
    }

    let mut fk_stmt = conn
        .prepare("PRAGMA foreign_key_check;")
        .map_err(|err| row_auto_merge_sqlite_err(path, "prepare foreign_key_check", err))?;
    let mut fk_rows = fk_stmt
        .query([])
        .map_err(|err| row_auto_merge_sqlite_err(path, "run foreign_key_check", err))?;
    if let Some(row) = fk_rows
        .next()
        .map_err(|err| row_auto_merge_sqlite_err(path, "read foreign_key_check", err))?
    {
        let table = row
            .get::<_, String>(0)
            .unwrap_or_else(|_| "<unknown>".into());
        let rowid = row.get::<_, Option<i64>>(1).unwrap_or(None);
        let parent = row
            .get::<_, String>(2)
            .unwrap_or_else(|_| "<unknown>".into());
        let fkid = row.get::<_, i64>(3).unwrap_or_default();
        return Err(ErrCtx::PragmaErr(
            format!(
                "row-level auto-merge failed foreign_key_check at `{}`: table={table}, rowid={}, parent={parent}, fkid={fkid}",
                path.display(),
                rowid
                    .map(|rowid| rowid.to_string())
                    .unwrap_or_else(|| "NULL".to_string())
            )
            .into(),
        ));
    }

    Ok(())
}

fn row_auto_merge_sqlite_err(path: &Path, action: &str, err: rusqlite::Error) -> ErrCtx {
    ErrCtx::PragmaErr(
        format!(
            "could not {action} for row-level auto-merge at `{}`: {err}",
            path.display()
        )
        .into(),
    )
}

fn json_remote_info(remote: RemoteInfo) -> JsonRemoteInfo {
    let url = remote_config_uri(&remote.config);
    JsonRemoteInfo {
        name: remote.name,
        config: remote.config,
        url,
    }
}

fn format_remote(remote: &RemoteInfo) -> String {
    format!(
        "Added remote '{}': {}",
        remote.name,
        remote_config_uri(&remote.config)
    )
}

fn format_remotes(remotes: &[RemoteInfo]) -> Result<String, ErrCtx> {
    if remotes.is_empty() {
        return Ok("No remotes configured.".to_string());
    }

    let mut f = String::new();
    for remote in remotes {
        writeln!(
            &mut f,
            "{}\t{}",
            remote.name,
            remote_config_uri(&remote.config)
        )?;
    }
    Ok(f)
}

fn format_remote_prune_outcome(outcome: &RemotePruneOutcome) -> Result<String, ErrCtx> {
    if outcome.branches.is_empty() {
        return Ok(format!(
            "Pruned {} (no stale remote-tracking branches)",
            outcome.remote
        ));
    }

    let mut f = String::new();
    writeln!(
        &mut f,
        "Pruned {} ({} {})",
        outcome.remote,
        outcome.branches.len(),
        pluralize!(outcome.branches.len(), "branch")
    )?;
    for branch in &outcome.branches {
        writeln!(&mut f, "  {}/{}", outcome.remote, branch)?;
    }
    Ok(f)
}

fn format_ls_remote(
    remote: &str,
    default_branch: Option<&str>,
    refs: &[RemoteBranchRef],
) -> Result<String, ErrCtx> {
    if refs.is_empty() {
        return Ok(format!("No refs found for {remote}."));
    }

    let mut f = String::new();
    if let Some(default_branch) = default_branch
        && let Some(reference) = refs
            .iter()
            .find(|reference| reference.branch == default_branch)
    {
        writeln!(&mut f, "{}\tHEAD", reference.head)?;
    }
    for reference in refs {
        writeln!(
            &mut f,
            "{}\trefs/heads/{}",
            reference.head, reference.branch
        )?;
    }
    Ok(f)
}

fn remote_config_uri(config: &RemoteConfig) -> String {
    match config {
        RemoteConfig::Memory => "memory".to_string(),
        RemoteConfig::Fs { root } => format!("fs://{root}"),
        RemoteConfig::S3Compatible { bucket, prefix, endpoint } => {
            let mut uri = prefix.as_ref().map_or_else(
                || format!("s3://{bucket}"),
                |prefix| format!("s3://{bucket}/{prefix}"),
            );
            if let Some(endpoint) = endpoint {
                uri.push_str("?endpoint=");
                uri.push_str(endpoint);
            }
            uri
        }
        RemoteConfig::Http { url, token_env } => {
            let mut uri = if let Some(rest) = url.strip_prefix("https://") {
                format!("graft+https://{rest}")
            } else if let Some(rest) = url.strip_prefix("http://") {
                format!("graft+http://{rest}")
            } else {
                url.clone()
            };
            if let Some(token_env) = token_env {
                uri.push_str("?token_env=");
                uri.push_str(token_env);
            }
            uri
        }
    }
}

fn format_repo_log(repo: &Repository) -> Result<String, ErrCtx> {
    let commits = repo.log()?;
    if commits.is_empty() {
        return Ok("No commits yet.".to_string());
    }

    let mut f = String::new();
    for commit in commits {
        writeln!(&mut f, "commit {}", commit.id)?;
        if let Some(parent) = commit.parent {
            writeln!(&mut f, "parent {parent}")?;
        }
        writeln!(&mut f, "date {}", format_unix_millis(commit.timestamp_ms))?;
        writeln!(&mut f)?;
        writeln!(&mut f, "    {}", commit.message)?;
        writeln!(&mut f)?;
    }
    Ok(f)
}

fn format_debug_log_lsn(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let volume = runtime.volume_get(&file.vid)?;
    let commits = runtime.volume_log(&file.vid)?;
    if commits.is_empty() {
        return Ok(format!("No storage commits yet for log {}.", volume.local));
    }

    let mut f = String::new();
    writeln!(&mut f, "log {}", volume.local)?;
    for commit in commits {
        writeln!(&mut f, "commit {}:{}", volume.local, commit.lsn)?;
        writeln!(&mut f, "page_count {}", commit.page_count)?;
        writeln!(&mut f, "changed_pages {}", commit.changed_pages)?;
        if let Some(segment) = commit.segment_id {
            writeln!(&mut f, "segment {}", segment.short())?;
        }
        if commit.is_checkpoint {
            writeln!(&mut f, "checkpoint true")?;
        }
        if let Some(timestamp) = commit.timestamp {
            writeln!(&mut f, "date {}", format_unix_millis(timestamp))?;
        }
        if let Some(message) = commit.message {
            writeln!(&mut f)?;
            writeln!(&mut f, "    {message}")?;
        }
        writeln!(&mut f)?;
    }
    Ok(f)
}

fn format_debug_show_lsn(runtime: &Runtime, logref: &LogRef) -> Result<String, ErrCtx> {
    let Some(commit) = runtime.get_commit(&logref.log, logref.lsn)? else {
        return pragma_err!("commit not found");
    };
    let log = &commit.log;
    let lsn = commit.lsn;
    let page_count = commit.page_count;
    let commit_hash = &commit.commit_hash;
    let segment_idx = &commit.segment_idx;
    let checkpoints = &commit.checkpoints;
    Ok(formatdoc!(
        "
            Commit @ {log}:{lsn}
            page_count: {page_count}
            commit_hash: {commit_hash:?}
            segment_idx: {segment_idx:#?}
            checkpoints: {checkpoints:?}
        "
    ))
}

fn format_volume_info(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let state = runtime.volume_get(&file.vid)?;
    let sync = state.sync().map_or_else(
        || "Never synced".into(),
        |sync| match sync.local_watermark {
            Some(local) => format!("L{local} | R{}", sync.remote),
            None => format!("R{}", sync.remote),
        },
    );
    let vid = state.vid;
    let local = state.local;
    let remote = state.remote;
    let snapshot = file.snapshot_or_latest()?;
    let page_count = file.page_count()?;
    let snapshot_size = PAGESIZE * page_count.to_usize();

    Ok(formatdoc!(
        "
            Volume: {vid}
            Local: {local}
            Remote: {remote}
            Last sync: {sync}
            Snapshot: {snapshot:?}
            Snapshot pages: {page_count}
            Snapshot size: {snapshot_size}
        "
    ))
}

fn format_volume_status(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let mut f = String::new();

    let tag = &file.tag;
    writeln!(&mut f, "On tag {tag}")?;

    let status = runtime.volume_status(&file.vid)?;
    let local_changes = status.local_status.changes();
    let remote_changes = status.remote_status.changes();

    writeln!(
        &mut f,
        indoc! {"
            Local Log {} is grafted to
            remote Log {}.
        "},
        status.local, status.remote,
    )?;

    match (local_changes, remote_changes) {
        (Some(local), Some(remote)) => {
            write!(
                &mut f,
                indoc! {"
                    The Volume and the remote have diverged,
                    and have {} and {} different commits each, respectively.
                "},
                local.len(),
                remote.len(),
            )?;
        }
        (Some(local), None) => {
            write!(
                &mut f,
                indoc! {"
                      The Volume is ahead of the remote by {} {}.
                        (use 'pragma graft_push' to push repository commits)
                "},
                local.len(),
                pluralize!(local.len(), "commit")
            )?;
        }
        (None, Some(remote)) => {
            writeln!(
                &mut f,
                indoc! {"
                      The Volume is behind the remote by {} {}.
                        (use 'pragma graft_pull' to pull repository commits)
                "},
                remote.len(),
                pluralize!(remote.len(), "commit")
            )?;
        }
        (None, None) => {
            write!(&mut f, "The Volume is up to date with the remote.")?;
        }
    }

    Ok(f)
}

fn format_volume_audit(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let snapshot = file.snapshot_or_latest()?;
    let missing_pages = runtime.snapshot_missing_pages(&snapshot)?;
    let pages = file.page_count()?.to_usize();
    if missing_pages.is_empty() {
        let checksum = runtime.snapshot_checksum(&snapshot)?;
        Ok(formatdoc!(
            "
                Cached {pages} of {pages} {} (100%%) from the remote Log.
                Checksum: {checksum}
            ",
            pluralize!(pages, "page"),
        ))
    } else {
        let missing = missing_pages.cardinality().to_usize();
        let have = pages - missing;
        let pct = (have as f64) / (pages as f64) * 100.0;
        Ok(formatdoc!(
            "
                Cached {have} of {pages} {} ({pct:.02}%%) from the remote Log.
                  (use 'pragma graft_debug_volume_hydrate' to fetch missing pages)
            ",
            pluralize!(pages, "page"),
        ))
    }
}

fn json_volume_audit(runtime: &Runtime, file: &VolFile) -> Result<JsonVolumeAudit, ErrCtx> {
    let snapshot = file.snapshot_or_latest()?;
    let missing_pages = runtime.snapshot_missing_pages(&snapshot)?;
    let total_pages = file.page_count()?.to_usize();
    let missing = missing_pages.cardinality().to_usize();
    let local_pages = total_pages.saturating_sub(missing);
    let percentage = if total_pages == 0 {
        100.0
    } else {
        (local_pages as f64) / (total_pages as f64) * 100.0
    };
    let checksum = if missing_pages.is_empty() {
        Some(runtime.snapshot_checksum(&snapshot)?.to_string())
    } else {
        None
    };
    Ok(JsonVolumeAudit {
        local_pages,
        total_pages,
        percentage,
        needs_hydrate: !missing_pages.is_empty(),
        checksum,
    })
}

fn fetch_or_pull(runtime: &Runtime, file: &mut VolFile, pull: bool) -> Result<String, ErrCtx> {
    let pre = runtime.volume_status(&file.vid)?;
    if pull {
        runtime.volume_pull(file.vid.clone())?;
    } else {
        runtime.fetch_log(pre.remote, None)?;
    }
    let post = runtime.volume_status(&file.vid)?;

    let mut f = String::new();

    if let Some(diff) = AheadStatus::new(post.remote_status.base, pre.remote_status.base).changes()
    {
        writeln!(
            &mut f,
            "Pulled LSNs {} into remote Log {}",
            diff.to_string(),
            post.remote
        )?;
    } else {
        writeln!(&mut f, "No changes to remote Log {}", post.remote)?;
    }

    Ok(f)
}

fn push(runtime: &Runtime, file: &mut VolFile) -> Result<String, ErrCtx> {
    let pre = runtime.volume_status(&file.vid)?;
    if let Some(changes) = pre.local_status.changes()
        && !changes.is_empty()
    {
        runtime.volume_push(file.vid.clone())?;
        let post = runtime.volume_status(&file.vid)?;

        let pushed = AheadStatus::new(post.local_status.base, pre.local_status.base).changes();

        Ok(formatdoc!(
            "
                Pushed LSNs {} from local Log {}
                to remote Log {} @ {}
            ",
            pushed.map_or("unknown".into(), |lsns| lsns.to_string()),
            post.local,
            post.remote,
            post.remote_status
                .base
                .map_or("unknown".into(), |l| l.to_string())
        ))
    } else {
        Ok("Everything up-to-date".to_string())
    }
}

fn format_tags(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let mut f = String::new();
    let mut tags = runtime.tag_iter();
    while let Some((tag, vid)) = tags.try_next()? {
        let status = runtime.volume_status(&vid)?;
        let local = &status.local;
        let remote = &status.remote;

        writedoc!(
            &mut f,
            "
                Tag: {tag}{}
                  Volume: {vid}
                    Local: {local}
                    Remote: {remote}
                    Status: {status}
            ",
            if tag == file.tag { " (current)" } else { "" }
        )?;
    }
    Ok(f)
}

fn format_volumes(runtime: &Runtime, file: &VolFile) -> Result<String, ErrCtx> {
    let mut f = String::new();
    let mut volumes = runtime.volume_iter();
    while let Some(volume) = volumes.try_next()? {
        let vid = volume.vid;
        let status = runtime.volume_status(&vid)?;
        let local = volume.local;
        let remote = volume.remote;

        writedoc!(
            &mut f,
            "
                Volume: {vid}{}
                  Local: {local}
                  Remote: {remote}
                  Status: {status}
            ",
            if vid == file.vid { " (current)" } else { "" }
        )?;
    }
    Ok(f)
}

fn json_volumes(runtime: &Runtime, file: &VolFile) -> Result<Vec<JsonVolumeListEntry>, ErrCtx> {
    let mut entries = Vec::new();
    let mut volumes = runtime.volume_iter();
    while let Some(volume) = volumes.try_next()? {
        let vid = volume.vid;
        let status = runtime.volume_status(&vid)?;
        entries.push(JsonVolumeListEntry {
            id: vid.to_string(),
            local: volume.local.to_string(),
            remote: volume.remote.to_string(),
            status: status.to_string(),
            current: vid == file.vid,
        });
    }
    Ok(entries)
}

fn volume_export(_runtime: &Runtime, file: &VolFile, path: PathBuf) -> Result<String, ErrCtx> {
    // Get a reader based on the current state of the VolFile
    let reader = file.reader()?;

    let page_count = reader.page_count();
    let total_pages = page_count.to_usize();

    write_volume_reader_to_path(&reader, &path)?;

    Ok(format!(
        "exported {} {}",
        total_pages,
        pluralize!(total_pages, "page")
    ))
}

/// Format unix millis as "YYYY-MM-DD HH:MM:SS" without external crate
fn format_unix_millis(ts: u64) -> String {
    let secs = (ts / 1000) as i64;
    let days = secs / 86400;
    // Algorithm from Howard Hinnant (C++ chrono)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + (era * 400);
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let day_secs = secs.rem_euclid(86400) as u32;
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        day_secs / 3600,
        (day_secs / 60) % 60,
        day_secs % 60
    )
}

fn format_debug_page_diff(diff: &graft::PageDiffResult) -> String {
    let mut f = String::new();
    writeln!(
        &mut f,
        "Diff between LSN {} and LSN {}:",
        diff.from_lsn, diff.to_lsn
    )
    .unwrap();
    writeln!(&mut f, "  Page count delta: {:+}", diff.page_count_delta).unwrap();
    writeln!(
        &mut f,
        "  Changed pages: {}",
        diff.added_or_modified_pages.cardinality()
    )
    .unwrap();

    if !diff.added_or_modified_pages.is_empty() {
        writeln!(&mut f, "  Page indices:").unwrap();
        for page_idx in diff.added_or_modified_pages.iter().take(20) {
            writeln!(&mut f, "    - Page {page_idx}").unwrap();
        }
        let remaining = diff
            .added_or_modified_pages
            .cardinality()
            .to_usize()
            .saturating_sub(20);
        if remaining > 0 {
            writeln!(&mut f, "    ... and {remaining} more").unwrap();
        }
    }

    f
}

/// Row-level diff implementation
fn row_diff_impl(
    runtime: &Runtime,
    file: &VolFile,
    from: LSN,
    to: LSN,
) -> Result<Option<String>, ErrCtx> {
    let mut output = String::new();

    // Call row-level diff
    let diff = crate::row_level_diff::row_level_diff(runtime, &file.vid, from, to)
        .map_err(|e| ErrCtx::PragmaErr(format!("Row diff error: {e:?}").into()))?;

    writeln!(&mut output, "Row-level diff from LSN {from} to LSN {to}")?;
    writeln!(&mut output)?;

    if diff.table_changes.is_empty() && diff.opaque_changes.is_empty() {
        writeln!(&mut output, "No table changes detected.")?;
        return Ok(Some(output));
    }

    writeln!(&mut output, "Changed tables: {}", diff.table_changes.len())?;
    writeln!(&mut output)?;

    // Show changes for each table
    for table in &diff.table_changes {
        writeln!(&mut output, "Table: {}", table.table_name)?;
        writeln!(&mut output, "  Changes: {}", table.changes.len())?;

        // Count change types
        let mut inserts = 0;
        let mut deletes = 0;
        let mut updates = 0;

        for change in &table.changes {
            match change {
                crate::row_level_diff::RowChange::Insert { .. } => inserts += 1,
                crate::row_level_diff::RowChange::Delete { .. } => deletes += 1,
                crate::row_level_diff::RowChange::Update { .. } => updates += 1,
            }
        }

        if inserts > 0 {
            writeln!(&mut output, "    +{inserts} inserts")?;
        }
        if deletes > 0 {
            writeln!(&mut output, "    -{deletes} deletes")?;
        }
        if updates > 0 {
            writeln!(&mut output, "    ~{updates} updates")?;
        }

        // Show details for first few changes
        for (i, change) in table.changes.iter().take(5).enumerate() {
            match change {
                crate::row_level_diff::RowChange::Insert { rowid, row } => {
                    writeln!(&mut output, "    [{}] INSERT rowid={}", i + 1, rowid)?;
                    writeln!(
                        &mut output,
                        "      values: {:?}",
                        row.values
                            .iter()
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )?;
                }
                crate::row_level_diff::RowChange::Delete { rowid, .. } => {
                    writeln!(&mut output, "    [{}] DELETE rowid={}", i + 1, rowid)?;
                }
                crate::row_level_diff::RowChange::Update { rowid, old_row, new_row } => {
                    writeln!(&mut output, "    [{}] UPDATE rowid={}", i + 1, rowid)?;
                    writeln!(
                        &mut output,
                        "      old: {:?}",
                        old_row
                            .values
                            .iter()
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )?;
                    writeln!(
                        &mut output,
                        "      new: {:?}",
                        new_row
                            .values
                            .iter()
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )?;
                }
            }
        }

        if table.changes.len() > 5 {
            writeln!(
                &mut output,
                "    ... and {} more changes",
                table.changes.len() - 5
            )?;
        }

        writeln!(&mut output)?;
    }

    if !diff.opaque_changes.is_empty() {
        writeln!(&mut output, "Opaque changes:")?;
        for change in &diff.opaque_changes {
            let owner = change
                .owner
                .as_ref()
                .map(|owner| format!(" owned by {owner}"))
                .unwrap_or_default();
            writeln!(
                &mut output,
                "  {} {} ({}{})",
                change.change.as_str(),
                change.name,
                change.reason.as_str(),
                owner
            )?;
        }
        writeln!(&mut output)?;
    }

    // Generate SQL script
    writeln!(&mut output, "-- SQL Script --")?;
    for table in &diff.table_changes {
        writeln!(&mut output, "{}", table.to_sql())?;
    }

    Ok(Some(output))
}

fn json_opaque_changes(
    changes: &[crate::row_level_diff::OpaqueChange],
) -> Vec<crate::json::JsonOpaqueChange> {
    changes
        .iter()
        .map(|change| crate::json::JsonOpaqueChange {
            name: change.name.clone(),
            change: change.change.as_str().to_string(),
            reason: change.reason.as_str().to_string(),
            owner: change.owner.clone(),
        })
        .collect()
}

fn json_diff_capabilities(diff: &crate::row_level_diff::RowLevelDiff) -> Vec<String> {
    diff.analysis
        .capabilities
        .iter()
        .map(|capability| capability.as_str().to_string())
        .collect()
}

fn json_diff_limitations(
    diff: &crate::row_level_diff::RowLevelDiff,
) -> Vec<crate::json::JsonDiffLimitation> {
    json_limitations(&diff.analysis.limitations)
}

fn json_limitations(
    limitations: &[crate::row_level_diff::RowLevelDiffLimitation],
) -> Vec<crate::json::JsonDiffLimitation> {
    limitations
        .iter()
        .map(|limitation| crate::json::JsonDiffLimitation {
            kind: limitation.kind.as_str().to_string(),
            subject: limitation.subject.clone(),
        })
        .collect()
}

fn json_repo_row_diff(
    runtime: &Runtime,
    repo: &Repository,
    diff: &RepoDiff,
) -> Result<crate::json::JsonRepoRowDiffResult, ErrCtx> {
    let paths = diff
        .paths
        .iter()
        .map(|path| crate::json::JsonRepoPathDiff {
            path: path.path.clone(),
            change: repo_file_change_label(path.change).to_string(),
            kind: repo_tracked_path_kind_json_label(path.kind).to_string(),
            storage: repo_path_storage_json_label(path.storage).to_string(),
        })
        .collect();
    let files = diff
        .files
        .iter()
        .map(|file| {
            let change = repo_file_change_label(file.change).to_string();
            let kind = repo_tracked_path_kind_json_label(file.kind).to_string();
            let storage = repo_path_storage_json_label(file.storage).to_string();
            match repo_file_row_diff(runtime, repo, file) {
                Ok(Some(row_diff)) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    change,
                    kind,
                    storage,
                    row_diff_available: true,
                    logical_status: row_diff.logical_status().as_str().to_string(),
                    capabilities: json_diff_capabilities(&row_diff),
                    limitations: json_diff_limitations(&row_diff),
                    message: None,
                    tables: json_table_changes(&row_diff.table_changes),
                    opaque_changes: json_opaque_changes(&row_diff.opaque_changes),
                }),
                Ok(None) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    change: change.clone(),
                    kind,
                    storage,
                    row_diff_available: false,
                    logical_status: "row_diff_unavailable".to_string(),
                    capabilities: Vec::new(),
                    limitations: Vec::new(),
                    message: Some(format!(
                        "row diff unavailable for {change} database snapshots"
                    )),
                    tables: Vec::new(),
                    opaque_changes: Vec::new(),
                }),
                Err(err) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    change: change.clone(),
                    kind,
                    storage,
                    row_diff_available: false,
                    logical_status: "row_diff_unavailable".to_string(),
                    capabilities: Vec::new(),
                    limitations: Vec::new(),
                    message: Some(format!(
                        "row diff unavailable for {change} database snapshots: {err}"
                    )),
                    tables: Vec::new(),
                    opaque_changes: Vec::new(),
                }),
            }
        })
        .collect::<Result<Vec<_>, ErrCtx>>()?;

    Ok(crate::json::JsonRepoRowDiffResult {
        from: diff.from.clone(),
        to: diff.to.clone(),
        paths,
        files,
    })
}

fn json_table_changes(
    changes: &[crate::row_level_diff::TableChanges],
) -> Vec<crate::json::JsonTableChanges> {
    changes
        .iter()
        .map(|table| crate::json::JsonTableChanges {
            name: table.table_name.clone(),
            columns: table.columns.clone(),
            changes: table.changes.iter().map(json_row_change).collect(),
        })
        .collect()
}

fn json_row_change(change: &crate::row_level_diff::RowChange) -> crate::json::JsonRowChange {
    match change {
        crate::row_level_diff::RowChange::Insert { rowid, row } => crate::json::JsonRowChange {
            op: "insert".into(),
            rowid: *rowid,
            values: row
                .values
                .iter()
                .map(crate::json::JsonRowChange::value_to_json)
                .collect(),
            old_values: None,
        },
        crate::row_level_diff::RowChange::Delete { rowid, row } => crate::json::JsonRowChange {
            op: "delete".into(),
            rowid: *rowid,
            values: row
                .values
                .iter()
                .map(crate::json::JsonRowChange::value_to_json)
                .collect(),
            old_values: None,
        },
        crate::row_level_diff::RowChange::Update { rowid, old_row, new_row } => {
            crate::json::JsonRowChange {
                op: "update".into(),
                rowid: *rowid,
                values: new_row
                    .values
                    .iter()
                    .map(crate::json::JsonRowChange::value_to_json)
                    .collect(),
                old_values: Some(
                    old_row
                        .values
                        .iter()
                        .map(crate::json::JsonRowChange::value_to_json)
                        .collect(),
                ),
            }
        }
    }
}

/// Count changes for JSON summary
fn count_changes_json(changes: &[crate::row_level_diff::RowChange]) -> (usize, usize, usize) {
    let mut inserts = 0;
    let mut deletes = 0;
    let mut updates = 0;
    for change in changes {
        match change {
            crate::row_level_diff::RowChange::Insert { .. } => inserts += 1,
            crate::row_level_diff::RowChange::Delete { .. } => deletes += 1,
            crate::row_level_diff::RowChange::Update { .. } => updates += 1,
        }
    }
    (inserts, deletes, updates)
}

/// Generate JSON volume info
fn json_volume_info(
    runtime: &Runtime,
    file: &VolFile,
) -> Result<crate::json::JsonVolumeInfo, ErrCtx> {
    let state = runtime.volume_get(&file.vid)?;
    let page_count = file.page_count()?;
    let snapshot_size_bytes =
        (graft::core::page::PAGESIZE.as_usize() as u64) * (page_count.to_usize() as u64);

    Ok(crate::json::JsonVolumeInfo {
        vid: state.vid.to_string(),
        local: state.local.to_string(),
        remote: state.remote.to_string(),
        page_count: page_count.to_u32(),
        snapshot_size_bytes,
        snapshot_pages: page_count.to_u32(),
    })
}

/// Local struct for table log entries (text and JSON output)
struct TableLogEntry {
    lsn: u64,
    timestamp_ms: Option<u64>,
    when: String,
    summary: String,
    detail: String,
}

/// Find all commits that modified a specific table by diffing adjacent LSN pairs.
fn table_log_entries(
    runtime: &Runtime,
    vid: &VolumeId,
    table: &str,
) -> Result<Vec<TableLogEntry>, ErrCtx> {
    let commits = runtime.volume_log(vid)?;
    if commits.len() < 2 {
        return Ok(vec![]);
    }

    // Parse schema once to find the table's pages.
    // We do one checkout to read the schema, then reuse for page-level checks.
    let table_pages = get_table_page_set(runtime, vid, table)?;

    let volume = runtime
        .volume_get(vid)
        .map_err(|e| ErrCtx::PragmaErr(format!("Volume error: {e:?}").into()))?;
    let log_id = volume.local;

    // Commits come newest-first; iterate adjacent pairs ascending.
    let mut results: Vec<(usize, &graft::CommitInfo)> = Vec::new();
    for i in (1..commits.len()).rev() {
        let from = &commits[i];
        let to = &commits[i - 1];

        // Fast page-level check: did *any* of the table's pages change?
        let diff = runtime
            .diff_commits(&log_id, from.lsn, to.lsn)
            .map_err(|e| ErrCtx::PragmaErr(format!("Diff error: {e:?}").into()))?;

        let changed = table_pages.iter().any(|&page_num| {
            graft::core::PageIdx::try_new(page_num)
                .is_some_and(|pi| diff.added_or_modified_pages.contains(pi))
        });
        if changed {
            results.push((i, to));
        }
    }

    // If nothing changed, return early — no expensive diff needed.
    if results.is_empty() {
        return Ok(vec![]);
    }

    // Now do row-level diffs ONLY for the detected commit pairs.
    let mut entries = Vec::new();
    for (i, to) in results {
        let from = &commits[i];
        let diff = crate::row_level_diff::row_level_diff(runtime, vid, from.lsn, to.lsn)
            .map_err(|e| ErrCtx::PragmaErr(format!("Diff error: {e:?}").into()))?;

        if let Some(tc) = diff.table_changes.iter().find(|t| t.table_name == table)
            && !tc.is_empty()
        {
            let (inserts, deletes, updates) = count_changes_json(&tc.changes);
            let mut parts = Vec::new();
            if inserts > 0 {
                parts.push(format!("+{inserts}"));
            }
            if deletes > 0 {
                parts.push(format!("-{deletes}"));
            }
            if updates > 0 {
                parts.push(format!("~{updates}"));
            }
            let detail = format!("{inserts} inserts, {deletes} deletes, {updates} updates");
            entries.push(TableLogEntry {
                lsn: to.lsn.to_u64(),
                timestamp_ms: to.timestamp,
                when: to
                    .timestamp
                    .map_or_else(|| "-".to_string(), format_unix_millis),
                summary: parts.join(" "),
                detail,
            });
        }
    }

    Ok(entries)
}

/// Get the set of page indices that belong to a table's B-tree.
fn get_table_page_set(runtime: &Runtime, vid: &VolumeId, table: &str) -> Result<Vec<u32>, ErrCtx> {
    // Get latest LSN from commit history
    let commits = runtime
        .volume_log(vid)
        .map_err(|e| ErrCtx::PragmaErr(format!("Volume error: {e:?}").into()))?;
    let latest_lsn = commits
        .first()
        .map(|c| c.lsn)
        .ok_or_else(|| ErrCtx::PragmaErr("No commits".into()))?;

    let co = runtime
        .volume_checkout(vid, latest_lsn)
        .map_err(|e| ErrCtx::PragmaErr(format!("Checkout error: {e:?}").into()))?;
    let co_vid = co.vid;
    let reader = runtime
        .volume_reader(co_vid.clone())
        .map_err(|e| ErrCtx::PragmaErr(format!("Reader error: {e:?}").into()))?;

    let scanner = crate::sqlite_parse::TableScanner::new(&reader)
        .map_err(|e| ErrCtx::PragmaErr(format!("Parse error: {e:?}").into()))?;
    let master = scanner
        .read_master_table()
        .map_err(|e| ErrCtx::PragmaErr(format!("Schema error: {e:?}").into()))?;

    let root_page = master
        .iter()
        .find(|e| e.name == table)
        .map_or(0, |e| e.root_page);

    let mut pages = Vec::new();
    if root_page > 0 {
        collect_btree_pages(&reader, root_page, &mut pages);
    }

    let _ = runtime.volume_delete(&co_vid);
    Ok(pages)
}

/// Recursively collect all page numbers in a table B-tree.
fn collect_btree_pages(
    reader: &graft::volume_reader::VolumeReader,
    page_num: u32,
    pages: &mut Vec<u32>,
) {
    if page_num == 0 || pages.contains(&page_num) {
        return;
    }
    pages.push(page_num);

    let page_idx = match graft::core::PageIdx::try_new(page_num) {
        Some(p) => p,
        None => return,
    };
    let Ok(page) = reader.read_page(page_idx) else {
        return;
    };
    let data = page.as_ref();

    if data.len() < 12 {
        return;
    }
    let page_type = data[0];
    let num_cells = u16::from_be_bytes([data[3], data[4]]) as usize;

    if page_type == 5 {
        // Interior table page: recurse into children
        let right_child = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        collect_btree_pages(reader, right_child, pages);
        for i in 0..num_cells {
            let ptr = 12 + i * 2;
            if ptr + 2 > data.len() {
                break;
            }
            let cell_off = u16::from_be_bytes([data[ptr], data[ptr + 1]]) as usize;
            if cell_off + 4 <= data.len() {
                let left = u32::from_be_bytes([
                    data[cell_off],
                    data[cell_off + 1],
                    data[cell_off + 2],
                    data[cell_off + 3],
                ]);
                collect_btree_pages(reader, left, pages);
            }
        }
    }
    // Leaf pages (13) have no children — nothing to recurse.
}

#[cfg(test)]
mod tests {
    use super::*;
    use graft::repo::{RepoConflictChange, RepoStagedChange, RepoStatusCounts, RepoWorktreeChange};

    #[test]
    fn legacy_volume_pragmas_are_debug_only() {
        let legacy = Pragma { name: "graft_volume_push", arg: None };
        assert!(GraftPragma::try_from(&legacy).is_err());

        let debug = Pragma {
            name: "graft_debug_volume_push",
            arg: None,
        };
        assert!(matches!(
            GraftPragma::try_from(&debug).unwrap(),
            GraftPragma::VolumePush
        ));
    }

    #[test]
    fn undocumented_repo_compat_pragmas_are_rejected() {
        for name in [
            "graft_repo_status",
            "graft_remove",
            "graft_branch_move",
            "graft_branch_set_upstream",
            "graft_branch_set_upstream_to",
            "graft_tag_rm",
            "graft_remote_rm",
            "graft_remote_mv",
        ] {
            let pragma = Pragma { name, arg: Some("app.db") };
            assert!(
                GraftPragma::try_from(&pragma).is_err(),
                "{name} should be rejected"
            );
        }

        let status = Pragma { name: "graft_status", arg: None };
        assert!(matches!(
            GraftPragma::try_from(&status).unwrap(),
            GraftPragma::Status { spec: StatusSpec { kind: None } }
        ));
        let json_status = Pragma {
            name: "graft_json_status",
            arg: Some("--kind sqlite"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_status).unwrap(),
            GraftPragma::JsonStatus {
                spec: StatusSpec {
                    kind: Some(RepoTrackedPathKind::SqliteDatabase),
                },
            }
        ));

        let json_init = Pragma { name: "graft_json_init", arg: None };
        assert!(matches!(
            GraftPragma::try_from(&json_init).unwrap(),
            GraftPragma::JsonRepoInit
        ));

        let remove = Pragma { name: "graft_rm", arg: Some("app.db") };
        assert!(matches!(
            GraftPragma::try_from(&remove).unwrap(),
            GraftPragma::Remove { .. }
        ));

        let json_remove = Pragma {
            name: "graft_json_rm",
            arg: Some("app.db"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_remove).unwrap(),
            GraftPragma::JsonRemove { .. }
        ));

        let json_commit = Pragma {
            name: "graft_json_commit",
            arg: Some("message"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_commit).unwrap(),
            GraftPragma::JsonCommit { .. }
        ));
    }

    #[test]
    fn json_log_status_mode_is_opt_in() {
        let legacy = Pragma { name: "graft_json_log", arg: None };
        assert!(matches!(
            GraftPragma::try_from(&legacy).unwrap(),
            GraftPragma::JsonLog { mode: JsonLogMode::LegacyArray }
        ));

        let with_status = Pragma {
            name: "graft_json_log",
            arg: Some("--with-status"),
        };
        assert!(matches!(
            GraftPragma::try_from(&with_status).unwrap(),
            GraftPragma::JsonLog { mode: JsonLogMode::WithStatus }
        ));

        let invalid = Pragma {
            name: "graft_json_log",
            arg: Some("--status"),
        };
        assert!(GraftPragma::try_from(&invalid).is_err());
    }

    #[test]
    fn json_config_list_status_mode_is_opt_in() {
        let legacy = Pragma {
            name: "graft_json_config_list",
            arg: None,
        };
        assert!(matches!(
            GraftPragma::try_from(&legacy).unwrap(),
            GraftPragma::JsonConfigList { mode: JsonConfigListMode::LegacyArray }
        ));

        let with_status = Pragma {
            name: "graft_json_config_list",
            arg: Some("--with-status"),
        };
        assert!(matches!(
            GraftPragma::try_from(&with_status).unwrap(),
            GraftPragma::JsonConfigList { mode: JsonConfigListMode::WithStatus }
        ));

        let invalid = Pragma {
            name: "graft_json_config_list",
            arg: Some("--status"),
        };
        assert!(GraftPragma::try_from(&invalid).is_err());
    }

    #[test]
    fn json_tags_status_mode_is_opt_in() {
        let legacy = Pragma { name: "graft_json_tags", arg: None };
        assert!(matches!(
            GraftPragma::try_from(&legacy).unwrap(),
            GraftPragma::JsonTags { mode: JsonTagsMode::LegacyArray }
        ));

        let with_status = Pragma {
            name: "graft_json_tags",
            arg: Some("--with-status"),
        };
        assert!(matches!(
            GraftPragma::try_from(&with_status).unwrap(),
            GraftPragma::JsonTags { mode: JsonTagsMode::WithStatus }
        ));

        let invalid = Pragma {
            name: "graft_json_tags",
            arg: Some("--status"),
        };
        assert!(GraftPragma::try_from(&invalid).is_err());
    }

    #[test]
    fn async_job_pragmas_are_parsed() {
        let fetch = Pragma {
            name: "graft_fetch_async",
            arg: Some("--all origin"),
        };
        assert!(matches!(
            GraftPragma::try_from(&fetch).unwrap(),
            GraftPragma::FetchAsync { remote: Some(_), all: true, .. }
        ));

        let json_fetch = Pragma {
            name: "graft_json_fetch_async",
            arg: Some("origin main"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_fetch).unwrap(),
            GraftPragma::JsonFetchAsync {
                remote: Some(_),
                branch: Some(_),
                mode: JsonFetchAsyncMode::LegacyId,
                ..
            }
        ));

        let json_fetch_with_status = Pragma {
            name: "graft_json_fetch_async",
            arg: Some("--with-status --all origin"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_fetch_with_status).unwrap(),
            GraftPragma::JsonFetchAsync {
                remote: Some(_),
                all: true,
                mode: JsonFetchAsyncMode::WithStatus,
                ..
            }
        ));

        let status = Pragma {
            name: "graft_job_status",
            arg: Some("graft-job-1"),
        };
        assert!(matches!(
            GraftPragma::try_from(&status).unwrap(),
            GraftPragma::JobStatus { .. }
        ));

        let json_status = Pragma {
            name: "graft_json_job_status",
            arg: Some("graft-job-1"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_status).unwrap(),
            GraftPragma::JsonJobStatus { .. }
        ));

        let result = Pragma {
            name: "graft_job_result",
            arg: Some("graft-job-1"),
        };
        assert!(matches!(
            GraftPragma::try_from(&result).unwrap(),
            GraftPragma::JobResult { .. }
        ));
    }

    #[test]
    fn parse_remote_add_supports_explicit_s3_endpoint() {
        let (name, config) = parse_remote_add(
            "origin s3_compatible://my-bucket/prod/app?endpoint=http://localhost:9000",
        )
        .unwrap();

        assert_eq!(name, "origin");
        assert_eq!(
            config,
            RemoteConfig::S3Compatible {
                bucket: "my-bucket".to_string(),
                prefix: Some("prod/app".to_string()),
                endpoint: Some("http://localhost:9000".to_string()),
            }
        );
        assert_eq!(
            remote_config_uri(&config),
            "s3://my-bucket/prod/app?endpoint=http://localhost:9000"
        );
    }

    #[test]
    fn parse_remote_add_rejects_unknown_s3_query_parameters() {
        assert!(parse_remote_add("origin s3://my-bucket/prod?region=auto").is_err());
    }

    #[test]
    fn parse_remote_add_supports_graft_http_remote() {
        let (name, config) = parse_remote_add(
            "origin graft+https://graft.example.com/api/graft/v1/repos/acme/app?token_env=GRAFT_TOKEN",
        )
        .unwrap();

        assert_eq!(name, "origin");
        assert_eq!(
            config,
            RemoteConfig::Http {
                url: "https://graft.example.com/api/graft/v1/repos/acme/app".to_string(),
                token_env: Some("GRAFT_TOKEN".to_string()),
            }
        );
        assert_eq!(
            remote_config_uri(&config),
            "graft+https://graft.example.com/api/graft/v1/repos/acme/app?token_env=GRAFT_TOKEN"
        );
    }

    #[test]
    fn parse_remote_add_rejects_unknown_graft_http_query_parameters() {
        assert!(
            parse_remote_add("origin graft+https://graft.example.com/api?token=secret").is_err()
        );
    }

    #[test]
    fn parse_remote_rename_requires_two_names() {
        assert_eq!(
            parse_remote_rename("origin upstream").unwrap(),
            ("origin".to_string(), "upstream".to_string())
        );
        assert!(parse_remote_rename("origin").is_err());
        assert!(parse_remote_rename("origin upstream backup").is_err());
    }

    #[test]
    fn parse_json_remote_pragmas() {
        let add = Pragma {
            name: "graft_json_remote_add",
            arg: Some("origin memory"),
        };
        assert!(matches!(
            GraftPragma::try_from(&add).unwrap(),
            GraftPragma::JsonRemoteAdd { name, config: RemoteConfig::Memory } if name == "origin"
        ));

        let remove = Pragma {
            name: "graft_json_remote_remove",
            arg: Some("origin"),
        };
        assert!(matches!(
            GraftPragma::try_from(&remove).unwrap(),
            GraftPragma::JsonRemoteRemove { name } if name == "origin"
        ));

        let rename = Pragma {
            name: "graft_json_remote_rename",
            arg: Some("origin upstream"),
        };
        assert!(matches!(
            GraftPragma::try_from(&rename).unwrap(),
            GraftPragma::JsonRemoteRename { old, new } if old == "origin" && new == "upstream"
        ));

        let get_url = Pragma {
            name: "graft_json_remote_get_url",
            arg: Some("origin"),
        };
        assert!(matches!(
            GraftPragma::try_from(&get_url).unwrap(),
            GraftPragma::JsonRemoteGetUrl { name } if name == "origin"
        ));

        let set_url = Pragma {
            name: "graft_json_remote_set_url",
            arg: Some("origin memory"),
        };
        assert!(matches!(
            GraftPragma::try_from(&set_url).unwrap(),
            GraftPragma::JsonRemoteSetUrl { name, config: RemoteConfig::Memory } if name == "origin"
        ));

        let prune = Pragma {
            name: "graft_json_remote_prune",
            arg: Some("origin"),
        };
        assert!(matches!(
            GraftPragma::try_from(&prune).unwrap(),
            GraftPragma::JsonRemotePrune { name } if name == "origin"
        ));

        let ls_remote = Pragma {
            name: "graft_json_ls_remote",
            arg: Some("origin"),
        };
        assert!(matches!(
            GraftPragma::try_from(&ls_remote).unwrap(),
            GraftPragma::JsonLsRemote { name } if name == "origin"
        ));

        let remotes = Pragma { name: "graft_json_remotes", arg: None };
        assert!(matches!(
            GraftPragma::try_from(&remotes).unwrap(),
            GraftPragma::JsonRemotes
        ));
    }

    #[test]
    fn parse_branch_list_mode_supports_remote_and_all() {
        assert_eq!(parse_branch_list_mode(None).unwrap(), BranchListMode::Local);
        assert_eq!(
            parse_branch_list_mode(Some("--remote")).unwrap(),
            BranchListMode::Remote
        );
        assert_eq!(
            parse_branch_list_mode(Some("-r")).unwrap(),
            BranchListMode::Remote
        );
        assert_eq!(
            parse_branch_list_mode(Some("--all")).unwrap(),
            BranchListMode::All
        );
        assert_eq!(
            parse_branch_list_mode(Some("-a")).unwrap(),
            BranchListMode::All
        );
        assert!(parse_branch_list_mode(Some("--remote origin")).is_err());
    }

    #[test]
    fn parse_repo_clone_arg_supports_default_branch_and_branch_flags() {
        assert_eq!(
            parse_repo_clone_arg("fs:///srv/graft/app").unwrap(),
            RepoCloneSpec {
                config: RemoteConfig::Fs { root: "/srv/graft/app".to_string() },
                branch: None,
            }
        );
        assert_eq!(
            parse_repo_clone_arg("fs:///srv/graft/app feature/search").unwrap(),
            RepoCloneSpec {
                config: RemoteConfig::Fs { root: "/srv/graft/app".to_string() },
                branch: Some("feature/search".to_string()),
            }
        );
        assert_eq!(
            parse_repo_clone_arg("--branch feature/search memory").unwrap(),
            RepoCloneSpec {
                config: RemoteConfig::Memory,
                branch: Some("feature/search".to_string()),
            }
        );
        assert_eq!(
            parse_repo_clone_arg("-b release/1.0 memory").unwrap(),
            RepoCloneSpec {
                config: RemoteConfig::Memory,
                branch: Some("release/1.0".to_string()),
            }
        );
        assert!(parse_repo_clone_arg("").is_err());
        assert!(parse_repo_clone_arg("--branch feature/search").is_err());
        assert!(parse_repo_clone_arg("memory feature/search extra").is_err());

        let json_clone = Pragma {
            name: "graft_json_clone",
            arg: Some("--branch feature/search memory"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_clone).unwrap(),
            GraftPragma::JsonRepoClone {
                spec: RepoCloneSpec {
                    config: RemoteConfig::Memory,
                    branch: Some(branch),
                }
            } if branch == "feature/search"
        ));
    }

    #[test]
    fn parse_remote_branch_arg_supports_all_remote() {
        assert_eq!(
            parse_remote_branch_arg(Some("--all origin")).unwrap(),
            RemoteBranchArg {
                remote: Some("origin".to_string()),
                branch: None,
                refspec: None,
                all: true,
                force: false,
            }
        );
        assert_eq!(
            parse_remote_branch_arg(Some("origin main")).unwrap(),
            RemoteBranchArg {
                remote: Some("origin".to_string()),
                branch: Some("main".to_string()),
                refspec: None,
                all: false,
                force: false,
            }
        );
        assert_eq!(
            parse_remote_branch_arg(Some("--force origin main")).unwrap(),
            RemoteBranchArg {
                remote: Some("origin".to_string()),
                branch: Some("main".to_string()),
                refspec: None,
                all: false,
                force: true,
            }
        );
        assert_eq!(
            parse_remote_branch_arg(Some("main:backup")).unwrap(),
            RemoteBranchArg {
                remote: None,
                branch: None,
                refspec: Some("main:backup".to_string()),
                all: false,
                force: false,
            }
        );
        assert_eq!(
            parse_remote_branch_arg(Some("origin refs/heads/*:refs/remotes/origin/review/*"))
                .unwrap(),
            RemoteBranchArg {
                remote: Some("origin".to_string()),
                branch: None,
                refspec: Some("refs/heads/*:refs/remotes/origin/review/*".to_string()),
                all: false,
                force: false,
            }
        );
        assert_eq!(
            parse_remote_branch_arg(Some("--all -f origin")).unwrap(),
            RemoteBranchArg {
                remote: Some("origin".to_string()),
                branch: None,
                refspec: None,
                all: true,
                force: true,
            }
        );
        assert!(parse_remote_branch_arg(Some("--all origin main")).is_err());
    }

    #[test]
    fn parse_debug_diff_lsn_arg_requires_two_log_refs() {
        let log: LogId = "74ggbzxuMf-2uAmM7FwXntwW".parse().unwrap();
        let (from, to) =
            parse_debug_diff_lsn_arg("74ggbzxuMf-2uAmM7FwXntwW:2 74ggbzxuMf-2uAmM7FwXntwW:3")
                .unwrap();

        assert_eq!(from, LogRef::new(log.clone(), LSN::new(2)));
        assert_eq!(to, LogRef::new(log, LSN::new(3)));
        assert!(parse_debug_diff_lsn_arg("74ggbzxuMf-2uAmM7FwXntwW:2").is_err());
        assert!(parse_debug_diff_lsn_arg("2 3").is_err());
    }

    #[test]
    fn parse_repo_diff_arg_supports_row_mode() {
        assert_eq!(
            parse_repo_diff_arg(Some("--rows")).unwrap(),
            RepoDiffSpec {
                mode: DiffMode::Rows,
                kind: None,
                target: RepoDiffTarget::Worktree { path: None },
            }
        );
        assert_eq!(
            parse_repo_diff_arg(Some("--rows --staged -- app.db")).unwrap(),
            RepoDiffSpec {
                mode: DiffMode::Rows,
                kind: None,
                target: RepoDiffTarget::Staged { path: Some("app.db".to_string()) },
            }
        );
        assert_eq!(
            parse_repo_diff_arg(Some("--kind db --staged")).unwrap(),
            RepoDiffSpec {
                mode: DiffMode::Default,
                kind: Some(RepoTrackedPathKind::SqliteDatabase),
                target: RepoDiffTarget::Staged { path: None },
            }
        );
        assert_eq!(
            parse_repo_diff_arg(Some("--rows HEAD~1 HEAD -- app.db")).unwrap(),
            RepoDiffSpec {
                mode: DiffMode::Rows,
                kind: None,
                target: RepoDiffTarget::Revisions {
                    from: "HEAD~1".to_string(),
                    to: "HEAD".to_string(),
                    path: Some("app.db".to_string()),
                },
            }
        );
        assert_eq!(
            parse_repo_diff_arg(Some("HEAD -- --rows")).unwrap(),
            RepoDiffSpec {
                mode: DiffMode::Default,
                kind: None,
                target: RepoDiffTarget::RevisionToWorktree {
                    rev: "HEAD".to_string(),
                    path: Some("--rows".to_string()),
                },
            }
        );
        assert!(parse_repo_diff_arg(Some("--rows --rows")).is_err());
        assert!(parse_repo_diff_arg(Some("--kind nope")).is_err());
        assert!(parse_repo_diff_arg(Some("--kind db --kind text_file")).is_err());
    }

    #[test]
    fn parse_repo_add_arg_supports_force() {
        assert_eq!(
            parse_repo_add_arg(None).unwrap(),
            RepoAddSpec {
                path: None,
                force: false,
                all: false,
                kind: None,
            }
        );
        assert_eq!(
            parse_repo_add_arg(Some("--all")).unwrap(),
            RepoAddSpec {
                path: None,
                force: false,
                all: true,
                kind: None,
            }
        );
        assert_eq!(
            parse_repo_add_arg(Some("-A")).unwrap(),
            RepoAddSpec {
                path: None,
                force: false,
                all: true,
                kind: None,
            }
        );
        assert_eq!(
            parse_repo_add_arg(Some("--all --kind db")).unwrap(),
            RepoAddSpec {
                path: None,
                force: false,
                all: true,
                kind: Some(RepoTrackedPathKind::SqliteDatabase),
            }
        );
        assert_eq!(
            parse_repo_add_arg(Some("--kind binary_file -A")).unwrap(),
            RepoAddSpec {
                path: None,
                force: false,
                all: true,
                kind: Some(RepoTrackedPathKind::BinaryFile),
            }
        );
        assert_eq!(
            parse_repo_add_arg(Some("assets/readme.md")).unwrap(),
            RepoAddSpec {
                path: Some(PathBuf::from("assets/readme.md")),
                force: false,
                all: false,
                kind: None,
            }
        );
        assert_eq!(
            parse_repo_add_arg(Some("--force -- assets/readme.md")).unwrap(),
            RepoAddSpec {
                path: Some(PathBuf::from("assets/readme.md")),
                force: true,
                all: false,
                kind: None,
            }
        );
        assert_eq!(
            parse_repo_add_arg(Some("-f assets/readme.md")).unwrap(),
            RepoAddSpec {
                path: Some(PathBuf::from("assets/readme.md")),
                force: true,
                all: false,
                kind: None,
            }
        );
        assert!(parse_repo_add_arg(Some("--force --all")).is_err());
        assert!(parse_repo_add_arg(Some("--kind db")).is_err());
        assert!(parse_repo_add_arg(Some("--all --kind nope")).is_err());
        assert!(parse_repo_add_arg(Some("--force --all --kind db")).is_err());
        assert!(parse_repo_add_arg(Some("--unknown assets/readme.md")).is_err());
    }

    #[test]
    fn parse_repo_remove_arg_supports_cached() {
        assert_eq!(
            parse_repo_remove_arg(None).unwrap(),
            RepoRemoveSpec { path: None, cached: false }
        );
        assert_eq!(
            parse_repo_remove_arg(Some("assets/readme.md")).unwrap(),
            RepoRemoveSpec {
                path: Some(PathBuf::from("assets/readme.md")),
                cached: false,
            }
        );
        assert_eq!(
            parse_repo_remove_arg(Some("--cached")).unwrap(),
            RepoRemoveSpec { path: None, cached: true }
        );
        assert_eq!(
            parse_repo_remove_arg(Some("--cached -- assets/readme.md")).unwrap(),
            RepoRemoveSpec {
                path: Some(PathBuf::from("assets/readme.md")),
                cached: true,
            }
        );
        assert!(parse_repo_remove_arg(Some("--cached --cached")).is_err());
        assert!(parse_repo_remove_arg(Some("--unknown assets/readme.md")).is_err());
    }

    #[test]
    fn parse_repo_audit_arg_supports_repair() {
        assert_eq!(
            parse_repo_audit_arg(None).unwrap(),
            RepoAuditSpec { repair: false, remote: None }
        );
        assert_eq!(
            parse_repo_audit_arg(Some("")).unwrap(),
            RepoAuditSpec { repair: false, remote: None }
        );
        assert_eq!(
            parse_repo_audit_arg(Some("--repair")).unwrap(),
            RepoAuditSpec { repair: true, remote: None }
        );
        assert_eq!(
            parse_repo_audit_arg(Some("--repair origin")).unwrap(),
            RepoAuditSpec {
                repair: true,
                remote: Some("origin".to_string()),
            }
        );
        assert!(parse_repo_audit_arg(Some("origin")).is_err());
        assert!(parse_repo_audit_arg(Some("--repair --repair")).is_err());
        assert!(parse_repo_audit_arg(Some("--unknown")).is_err());
    }

    #[test]
    fn parse_lfs_fetch_arg_supports_remote_and_revision() {
        assert_eq!(
            parse_lfs_fetch_arg(None).unwrap(),
            LargeFileFetchSpec { remote: None, rev: None }
        );
        assert_eq!(
            parse_lfs_fetch_arg(Some("")).unwrap(),
            LargeFileFetchSpec { remote: None, rev: None }
        );
        assert_eq!(
            parse_lfs_fetch_arg(Some("HEAD~1")).unwrap(),
            LargeFileFetchSpec {
                remote: None,
                rev: Some("HEAD~1".to_string())
            }
        );
        assert_eq!(
            parse_lfs_fetch_arg(Some("--remote origin origin/main")).unwrap(),
            LargeFileFetchSpec {
                remote: Some("origin".to_string()),
                rev: Some("origin/main".to_string())
            }
        );
        assert!(parse_lfs_fetch_arg(Some("--remote")).is_err());
        assert!(parse_lfs_fetch_arg(Some("--remote origin --remote upstream")).is_err());
        assert!(parse_lfs_fetch_arg(Some("HEAD main")).is_err());
        assert!(parse_lfs_fetch_arg(Some("--unknown")).is_err());
    }

    #[test]
    fn parse_lfs_status_arg_supports_optional_revision() {
        assert_eq!(
            parse_lfs_status_arg(None).unwrap(),
            LargeFileStatusSpec { rev: None }
        );
        assert_eq!(
            parse_lfs_status_arg(Some("")).unwrap(),
            LargeFileStatusSpec { rev: None }
        );
        assert_eq!(
            parse_lfs_status_arg(Some("HEAD~1")).unwrap(),
            LargeFileStatusSpec { rev: Some("HEAD~1".to_string()) }
        );
        assert!(parse_lfs_status_arg(Some("HEAD main")).is_err());
        assert!(parse_lfs_status_arg(Some("--unknown")).is_err());
    }

    #[test]
    fn parse_lfs_prune_arg_defaults_to_dry_run() {
        assert_eq!(
            parse_lfs_prune_arg(None).unwrap(),
            LargeFilePruneSpec { dry_run: true }
        );
        assert_eq!(
            parse_lfs_prune_arg(Some("")).unwrap(),
            LargeFilePruneSpec { dry_run: true }
        );
        assert_eq!(
            parse_lfs_prune_arg(Some("--dry-run")).unwrap(),
            LargeFilePruneSpec { dry_run: true }
        );
        assert_eq!(
            parse_lfs_prune_arg(Some("--force")).unwrap(),
            LargeFilePruneSpec { dry_run: false }
        );
        assert!(parse_lfs_prune_arg(Some("--force --dry-run")).is_err());
        assert!(parse_lfs_prune_arg(Some("--unknown")).is_err());
    }

    #[test]
    fn payload_pragmas_alias_lfs_payload_pragmas() {
        assert!(matches!(
            GraftPragma::try_from(&Pragma {
                name: "graft_payload_fetch",
                arg: Some("--remote origin HEAD")
            })
            .unwrap(),
            GraftPragma::LargeFileFetch { .. }
        ));
        assert!(matches!(
            GraftPragma::try_from(&Pragma {
                name: "graft_json_payload_status",
                arg: Some("HEAD")
            })
            .unwrap(),
            GraftPragma::JsonLargeFileStatus { .. }
        ));
        assert!(matches!(
            GraftPragma::try_from(&Pragma {
                name: "graft_payload_prune",
                arg: Some("--dry-run")
            })
            .unwrap(),
            GraftPragma::LargeFilePrune { .. }
        ));
    }

    #[test]
    fn parse_ls_files_arg_supports_stage() {
        assert_eq!(
            parse_ls_files_arg(None).unwrap(),
            LsFilesSpec {
                stage: false,
                details: false,
                others: false,
                kind: None
            }
        );
        assert_eq!(
            parse_ls_files_arg(Some("")).unwrap(),
            LsFilesSpec {
                stage: false,
                details: false,
                others: false,
                kind: None
            }
        );
        assert_eq!(
            parse_ls_files_arg(Some("--stage")).unwrap(),
            LsFilesSpec {
                stage: true,
                details: false,
                others: false,
                kind: None
            }
        );
        assert_eq!(
            parse_ls_files_arg(Some("-s")).unwrap(),
            LsFilesSpec {
                stage: true,
                details: false,
                others: false,
                kind: None
            }
        );
        assert_eq!(
            parse_ls_files_arg(Some("--kind sqlite")).unwrap(),
            LsFilesSpec {
                stage: false,
                details: false,
                others: false,
                kind: Some(RepoTrackedPathKind::SqliteDatabase),
            }
        );
        assert_eq!(
            parse_ls_files_arg(Some("--stage --kind binary_file")).unwrap(),
            LsFilesSpec {
                stage: true,
                details: false,
                others: false,
                kind: Some(RepoTrackedPathKind::BinaryFile),
            }
        );
        assert_eq!(
            parse_ls_files_arg(Some("--details --kind text")).unwrap(),
            LsFilesSpec {
                stage: false,
                details: true,
                others: false,
                kind: Some(RepoTrackedPathKind::TextFile),
            }
        );
        assert_eq!(
            parse_ls_files_arg(Some("--others --kind binary")).unwrap(),
            LsFilesSpec {
                stage: false,
                details: false,
                others: true,
                kind: Some(RepoTrackedPathKind::BinaryFile),
            }
        );
        assert!(parse_ls_files_arg(Some("--kind binary -s")).is_ok());
        assert!(parse_ls_files_arg(Some("--unknown")).is_err());
        assert!(parse_ls_files_arg(Some("--kind nope")).is_err());
        assert!(parse_ls_files_arg(Some("--stage --stage")).is_err());
        assert!(parse_ls_files_arg(Some("--details --details")).is_err());
        assert!(parse_ls_files_arg(Some("--others --others")).is_err());
        assert!(parse_ls_files_arg(Some("--stage --details")).is_err());
        assert!(parse_ls_files_arg(Some("--stage --others")).is_err());
        assert!(parse_ls_files_arg(Some("--details --others")).is_err());
    }

    #[test]
    fn parse_status_arg_supports_kind_filter() {
        assert_eq!(parse_status_arg(None).unwrap(), StatusSpec { kind: None });
        assert_eq!(
            parse_status_arg(Some("")).unwrap(),
            StatusSpec { kind: None }
        );
        assert_eq!(
            parse_status_arg(Some("--kind db")).unwrap(),
            StatusSpec {
                kind: Some(RepoTrackedPathKind::SqliteDatabase)
            }
        );
        assert_eq!(
            parse_status_arg(Some("--kind binary_file")).unwrap(),
            StatusSpec {
                kind: Some(RepoTrackedPathKind::BinaryFile)
            }
        );
        assert!(parse_status_arg(Some("--kind nope")).is_err());
        assert!(parse_status_arg(Some("--kind text_file --kind binary_file")).is_err());
        assert!(parse_status_arg(Some("--stage")).is_err());
    }

    #[test]
    fn status_kind_filter_recomputes_paths_and_counts() {
        let status = RepoStatus {
            worktree: PathBuf::from("/repo"),
            graft_dir: PathBuf::from("/repo/.graft"),
            repository_format_version: 1,
            head: Head::branch("main"),
            head_target: Some("head".to_string()),
            merge_head: None,
            orig_head: None,
            dirty: true,
            has_unstaged_changes: true,
            has_staged_changes: true,
            has_conflicts: true,
            work_in_progress: true,
            counts: RepoStatusCounts { unstaged: 2, staged: 1, conflicted: 1 },
            paths: Vec::new(),
            unstaged: vec!["app.db".to_string(), "assets/readme.md".to_string()],
            unstaged_changes: vec![
                RepoWorktreeChange {
                    path: "app.db".to_string(),
                    change: RepoWorktreeChangeKind::Modified,
                    kind: RepoTrackedPathKind::SqliteDatabase,
                    storage: RepoPathStorage::SqliteSnapshot,
                },
                RepoWorktreeChange {
                    path: "assets/readme.md".to_string(),
                    change: RepoWorktreeChangeKind::Modified,
                    kind: RepoTrackedPathKind::TextFile,
                    storage: RepoPathStorage::Inline,
                },
            ],
            staged: vec!["app.db".to_string()],
            staged_changes: vec![RepoStagedChange {
                path: "app.db".to_string(),
                change: RepoFileChange::Modified,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
            }],
            conflicted: vec!["assets/model.bin".to_string()],
            conflicted_changes: vec![RepoConflictChange {
                path: "assets/model.bin".to_string(),
                kind: RepoTrackedPathKind::BinaryFile,
                storage: RepoPathStorage::External,
            }],
            branches: Vec::new(),
            remotes: Vec::new(),
            upstream: None,
            upstream_status: None,
            ahead: 0,
            behind: 0,
        };

        let sqlite_status =
            filter_repo_status_by_kind(status.clone(), Some(RepoTrackedPathKind::SqliteDatabase));
        assert_eq!(sqlite_status.unstaged, vec!["app.db"]);
        assert_eq!(sqlite_status.staged, vec!["app.db"]);
        assert!(sqlite_status.conflicted.is_empty());
        assert_eq!(sqlite_status.counts.unstaged, 1);
        assert_eq!(sqlite_status.counts.staged, 1);
        assert_eq!(sqlite_status.counts.conflicted, 0);
        assert!(sqlite_status.has_unstaged_changes);
        assert!(sqlite_status.has_staged_changes);
        assert!(!sqlite_status.has_conflicts);
        assert_eq!(sqlite_status.paths.len(), 1);
        assert_eq!(
            sqlite_status.paths[0].kind,
            RepoTrackedPathKind::SqliteDatabase
        );

        let binary_status =
            filter_repo_status_by_kind(status, Some(RepoTrackedPathKind::BinaryFile));
        assert!(binary_status.unstaged.is_empty());
        assert!(binary_status.staged.is_empty());
        assert_eq!(binary_status.conflicted, vec!["assets/model.bin"]);
        assert_eq!(binary_status.counts.conflicted, 1);
        assert!(binary_status.has_conflicts);
        assert_eq!(binary_status.paths.len(), 1);
        assert_eq!(binary_status.paths[0].kind, RepoTrackedPathKind::BinaryFile);
        assert_eq!(binary_status.paths[0].storage, RepoPathStorage::External);
    }

    #[test]
    fn ls_files_kind_filter_selects_matching_path_kinds() {
        let paths = vec![
            RepoTrackedPath {
                path: "app.db".to_string(),
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
                size: None,
                page_count: None,
            },
            RepoTrackedPath {
                path: "assets/readme.md".to_string(),
                kind: RepoTrackedPathKind::TextFile,
                storage: RepoPathStorage::Inline,
                size: Some(12),
                page_count: None,
            },
            RepoTrackedPath {
                path: "assets/model.bin".to_string(),
                kind: RepoTrackedPathKind::BinaryFile,
                storage: RepoPathStorage::External,
                size: Some(4096),
                page_count: None,
            },
        ];
        let sqlite_paths =
            filter_tracked_paths_by_kind(paths, Some(RepoTrackedPathKind::SqliteDatabase));
        assert_eq!(sqlite_paths.len(), 1);
        assert_eq!(sqlite_paths[0].path, "app.db");

        let entries = vec![
            RepoTrackedPathEntry {
                path: "app.db".to_string(),
                stage: graft::repo::index::IndexStage::Normal,
                kind: RepoTrackedPathKind::SqliteDatabase,
                storage: RepoPathStorage::SqliteSnapshot,
                mode: None,
                oid: None,
                size: None,
                page_count: None,
            },
            RepoTrackedPathEntry {
                path: "assets/model.bin".to_string(),
                stage: graft::repo::index::IndexStage::Normal,
                kind: RepoTrackedPathKind::BinaryFile,
                storage: RepoPathStorage::External,
                mode: None,
                oid: None,
                size: Some(4096),
                page_count: None,
            },
        ];
        let binary_entries =
            filter_tracked_path_entries_by_kind(entries, Some(RepoTrackedPathKind::BinaryFile));
        assert_eq!(binary_entries.len(), 1);
        assert_eq!(binary_entries[0].path, "assets/model.bin");
    }

    #[test]
    fn parse_repo_config_set_arg_preserves_values_with_spaces() {
        assert_eq!(
            parse_repo_config_set_arg("files.inline_text_threshold -- 8 MB").unwrap(),
            (
                "files.inline_text_threshold".to_string(),
                "8 MB".to_string()
            )
        );
        assert_eq!(
            parse_repo_config_set_arg("files.inline_text_threshold 4 B").unwrap(),
            ("files.inline_text_threshold".to_string(), "4 B".to_string())
        );
        assert!(parse_repo_config_set_arg("files.inline_text_threshold -- ").is_err());
        assert!(parse_repo_config_set_arg("").is_err());
    }

    #[test]
    fn parse_tag_create_arg_supports_annotated_messages() {
        assert_eq!(
            parse_tag_create_arg("v1.0 HEAD").unwrap(),
            ("v1.0".to_string(), Some("HEAD".to_string()), None)
        );
        assert_eq!(
            parse_tag_create_arg("--annotated v1.0 HEAD -- release 1.0").unwrap(),
            (
                "v1.0".to_string(),
                Some("HEAD".to_string()),
                Some("release 1.0".to_string())
            )
        );
        assert_eq!(
            parse_tag_create_arg("-a v1.0 -- release 1.0").unwrap(),
            ("v1.0".to_string(), None, Some("release 1.0".to_string()))
        );
        assert!(parse_tag_create_arg("--annotated v1.0 HEAD").is_err());
        assert!(parse_tag_create_arg("--annotated v1.0 HEAD -- ").is_err());
    }

    #[test]
    fn parse_json_tag_pragmas() {
        let create = Pragma {
            name: "graft_json_tag_create",
            arg: Some("v1.0 HEAD"),
        };
        assert!(matches!(
            GraftPragma::try_from(&create).unwrap(),
            GraftPragma::JsonTagCreate {
                name,
                target: Some(target),
                message: None,
            } if name == "v1.0" && target == "HEAD"
        ));

        let annotated = Pragma {
            name: "graft_json_tag_create",
            arg: Some("--annotated v1.0 HEAD -- release 1.0"),
        };
        assert!(matches!(
            GraftPragma::try_from(&annotated).unwrap(),
            GraftPragma::JsonTagCreate {
                name,
                target: Some(target),
                message: Some(message),
            } if name == "v1.0" && target == "HEAD" && message == "release 1.0"
        ));

        let delete = Pragma {
            name: "graft_json_tag_delete",
            arg: Some("v1.0"),
        };
        assert!(matches!(
            GraftPragma::try_from(&delete).unwrap(),
            GraftPragma::JsonTagDelete { name } if name == "v1.0"
        ));
    }

    #[test]
    fn parse_checkout_and_switch_force_args() {
        assert_eq!(
            parse_repo_checkout_arg("--force HEAD~1").unwrap(),
            RepoCheckoutSpec::Detach { rev: "HEAD~1".to_string(), force: true }
        );
        assert_eq!(
            parse_repo_checkout_arg("HEAD~1 -- app.db").unwrap(),
            RepoCheckoutSpec::Path {
                rev: "HEAD~1".to_string(),
                path: "app.db".to_string(),
            }
        );
        assert!(parse_repo_checkout_arg("--force HEAD~1 -- app.db").is_err());
        assert_eq!(
            parse_repo_restore_arg("external.db").unwrap(),
            RepoRestoreSpec {
                source: None,
                staged: false,
                all: false,
                kind: None,
                path: Some(PathBuf::from("external.db")),
            }
        );
        assert_eq!(
            parse_repo_restore_arg("--source HEAD~1 -- external.db").unwrap(),
            RepoRestoreSpec {
                source: Some("HEAD~1".to_string()),
                staged: false,
                all: false,
                kind: None,
                path: Some(PathBuf::from("external.db")),
            }
        );
        assert_eq!(
            parse_repo_restore_arg("--staged -- external.db").unwrap(),
            RepoRestoreSpec {
                source: None,
                staged: true,
                all: false,
                kind: None,
                path: Some(PathBuf::from("external.db")),
            }
        );
        assert_eq!(
            parse_repo_restore_arg("--staged --source HEAD -- external.db").unwrap(),
            RepoRestoreSpec {
                source: Some("HEAD".to_string()),
                staged: true,
                all: false,
                kind: None,
                path: Some(PathBuf::from("external.db")),
            }
        );
        assert_eq!(
            parse_repo_restore_arg("--staged --all --kind db").unwrap(),
            RepoRestoreSpec {
                source: None,
                staged: true,
                all: true,
                kind: Some(RepoTrackedPathKind::SqliteDatabase),
                path: None,
            }
        );
        assert!(parse_repo_restore_arg("--all").is_err());
        assert!(parse_repo_restore_arg("--kind db -- external.db").is_err());
        assert!(parse_repo_restore_arg("--staged --all external.db").is_err());
        assert!(parse_repo_restore_arg("--staged --all --kind nope").is_err());
        assert_eq!(
            parse_repo_export_arg("--output snapshot.db").unwrap(),
            RepoExportSpec {
                source: None,
                path: None,
                output: PathBuf::from("snapshot.db"),
            }
        );
        assert_eq!(
            parse_repo_export_arg("--source HEAD~1 --output snapshot.db -- app.db").unwrap(),
            RepoExportSpec {
                source: Some("HEAD~1".to_string()),
                path: Some(PathBuf::from("app.db")),
                output: PathBuf::from("snapshot.db"),
            }
        );
        assert_eq!(
            parse_repo_export_arg("--output '/tmp/Application Support/snapshot.db'").unwrap(),
            RepoExportSpec {
                source: None,
                path: None,
                output: PathBuf::from("/tmp/Application Support/snapshot.db"),
            }
        );
        assert_eq!(
            parse_repo_export_arg(
                "--source HEAD --output \"/tmp/Application Support/snapshot.db\" -- \"app data.db\""
            )
            .unwrap(),
            RepoExportSpec {
                source: Some("HEAD".to_string()),
                path: Some(PathBuf::from("app data.db")),
                output: PathBuf::from("/tmp/Application Support/snapshot.db"),
            }
        );
        assert_eq!(
            parse_repo_export_arg("fixtures/users.db -o snapshot.db").unwrap(),
            RepoExportSpec {
                source: None,
                path: Some(PathBuf::from("fixtures/users.db")),
                output: PathBuf::from("snapshot.db"),
            }
        );
        assert!(parse_repo_export_arg("--source HEAD").is_err());

        let json_export = Pragma {
            name: "graft_json_export",
            arg: Some("--source HEAD --output snapshot.db -- app.db"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_export).unwrap(),
            GraftPragma::JsonExport {
                spec: RepoExportSpec {
                    source: Some(source),
                    path: Some(path),
                    output,
                }
            } if source == "HEAD"
                && path == PathBuf::from("app.db")
                && output == PathBuf::from("snapshot.db")
        ));

        assert_eq!(
            parse_switch_branch_arg("--force main").unwrap(),
            ("main".to_string(), true)
        );
        assert_eq!(
            parse_branch_rename_arg("feature/query").unwrap(),
            (None, "feature/query".to_string(), false)
        );
        assert_eq!(
            parse_branch_rename_arg("feature/search feature/query").unwrap(),
            (
                Some("feature/search".to_string()),
                "feature/query".to_string(),
                false
            )
        );
        assert_eq!(
            parse_branch_rename_arg("-M feature/search feature/query").unwrap(),
            (
                Some("feature/search".to_string()),
                "feature/query".to_string(),
                true
            )
        );
        assert_eq!(
            parse_switch_create_arg("-f feature/search HEAD").unwrap(),
            ("feature/search".to_string(), Some("HEAD".to_string()), true)
        );
        let json_switch_branch = Pragma {
            name: "graft_json_switch_branch",
            arg: Some("--force main"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_switch_branch).unwrap(),
            GraftPragma::JsonSwitchBranch { name, force: true } if name == "main"
        ));
        let json_switch_create = Pragma {
            name: "graft_json_switch_create",
            arg: Some("-f feature/search HEAD"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_switch_create).unwrap(),
            GraftPragma::JsonSwitchCreate {
                name,
                start_point: Some(start_point),
                force: true,
            } if name == "feature/search" && start_point == "HEAD"
        ));
        let json_branch_create = Pragma {
            name: "graft_json_branch_create",
            arg: Some("feature/search HEAD"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_branch_create).unwrap(),
            GraftPragma::JsonBranchCreate {
                name,
                start_point: Some(start_point),
            } if name == "feature/search" && start_point == "HEAD"
        ));
        let json_branch_delete = Pragma {
            name: "graft_json_branch_delete",
            arg: Some("--force feature/search"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_branch_delete).unwrap(),
            GraftPragma::JsonBranchDelete { name, force: true } if name == "feature/search"
        ));
        let json_branch_rename = Pragma {
            name: "graft_json_branch_rename",
            arg: Some("feature/search feature/query"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_branch_rename).unwrap(),
            GraftPragma::JsonBranchRename {
                old: Some(old),
                new,
                force: false,
            } if old == "feature/search" && new == "feature/query"
        ));
        let json_branch_upstream = Pragma {
            name: "graft_json_branch_upstream",
            arg: Some("feature/query origin/main"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_branch_upstream).unwrap(),
            GraftPragma::JsonBranchUpstream {
                branch: Some(branch),
                remote,
                remote_branch,
            } if branch == "feature/query" && remote == "origin" && remote_branch == "main"
        ));
        let json_branch_unset_upstream = Pragma {
            name: "graft_json_branch_unset_upstream",
            arg: Some("feature/query"),
        };
        assert!(matches!(
            GraftPragma::try_from(&json_branch_unset_upstream).unwrap(),
            GraftPragma::JsonBranchUnsetUpstream { branch: Some(branch) }
                if branch == "feature/query"
        ));
    }
}
