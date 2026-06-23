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
    BranchInfo, BranchUpstream, CheckoutPlan, CommitFileState, CommitObject, CommitTableSummary,
    FetchAllOutcome, FetchOutcome, Head, MergeOutcome, MergePlan, PullOutcome, PushAllOutcome,
    PushOutcome, RemoteBranchRef, RemoteInfo, RemotePruneOutcome, RepoDiff, RepoFileChange,
    RepoLogRange, RepoSnapshot, RepoStatus, RepoStorageCommit, RepoWorktreeChangeKind, Repository,
    ResetMode, ResetOutcome, TagInfo,
};
use graft::{
    rt::runtime::Runtime, volume::AheadStatus, volume_reader::VolumeRead,
    volume_writer::VolumeWrite,
};
use indoc::{formatdoc, indoc, writedoc};
use parking_lot::Mutex;
use rusqlite::config::DbConfig;
use serde::Serialize;
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
            .insert(id.clone(), AsyncJob::running("fetch"));

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

fn unknown_job(id: &str) -> ErrCtx {
    ErrCtx::PragmaErr(format!("unknown job `{id}`").into())
}

#[derive(Debug, Clone)]
struct AsyncJob {
    kind: &'static str,
    state: AsyncJobState,
    result: Option<String>,
    error: Option<String>,
}

impl AsyncJob {
    fn running(kind: &'static str) -> Self {
        Self {
            kind,
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

#[derive(Debug, Clone, Serialize)]
struct JsonBranchList {
    branches: Vec<BranchInfo>,
    remote_branches: Vec<RemoteBranchRef>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonFetchCommandOutcome {
    operation: &'static str,
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

impl Serialize for FetchCommandOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        JsonFetchCommandOutcome {
            operation: "fetch",
            remote: self.remote(),
            branches: self.branches(),
            commits: self.commits(),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize)]
struct JsonPushCommandOutcome {
    operation: &'static str,
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

impl Serialize for PushCommandOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        JsonPushCommandOutcome {
            operation: "push",
            remote: self.remote(),
            branches: self.branches(),
            commits: self.commits(),
            forced: self.forced(),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize)]
struct JsonPullCommandOutcome {
    operation: &'static str,
    #[serde(flatten)]
    outcome: PullOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_analysis: Option<JsonRowMergeAnalysis>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRepoStatus {
    #[serde(flatten)]
    status: RepoStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_analysis: Option<JsonRowMergeAnalysis>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonCheckoutOutcome {
    operation: &'static str,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonResetCommandOutcome {
    operation: &'static str,
    #[serde(flatten)]
    outcome: ResetOutcome,
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
struct JsonRowMergeConflict {
    table: String,
    rowid: i64,
    ours: &'static str,
    theirs: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct JsonSchemaMergeConflict {
    name: String,
    entry_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ours: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theirs: Option<&'static str>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoDiffSpec {
    mode: DiffMode,
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
pub(crate) enum RepoCheckoutSpec {
    Detach { rev: String, force: bool },
    Path { rev: String, path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoRestoreSpec {
    source: Option<String>,
    staged: bool,
    path: PathBuf,
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
}

impl ResolveSide {
    fn index_stage(self) -> graft::repo::index::IndexStage {
        match self {
            Self::Ours => graft::repo::index::IndexStage::Ours,
            Self::Theirs => graft::repo::index::IndexStage::Theirs,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ours => "ours",
            Self::Theirs => "theirs",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoResolveSpec {
    side: ResolveSide,
    path: Option<PathBuf>,
}

pub(crate) enum GraftPragma {
    /// `pragma graft_debug_volume_list;`
    VolumeList,

    /// `pragma graft_debug_volume_json_list;`
    VolumeJsonList,

    /// `pragma graft_tags;`
    Tags,

    /// `pragma graft_json_tags;`
    JsonTags,

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

    /// `pragma graft_restore = "[--source rev] path";`
    Restore { spec: RepoRestoreSpec },

    /// `pragma graft_export = "[--source rev] --output output.db [-- path]";`
    Export { spec: RepoExportSpec },

    /// `pragma graft_debug_volume_info;`
    VolumeInfo,

    /// `pragma graft_status;`
    Status,

    /// `pragma graft_debug_volume_status;`
    VolumeStatus,

    /// `pragma graft_init;`
    RepoInit,

    /// `pragma graft_clone = "remote-uri [branch]";`
    RepoClone { spec: RepoCloneSpec },

    /// `pragma graft_json_status;`
    JsonStatus,

    /// `pragma graft_add;`
    Add { path: Option<PathBuf> },

    /// `pragma graft_rm;`
    Remove { path: Option<PathBuf> },

    /// `pragma graft_commit = "message";`
    Commit { message: String },

    /// `pragma graft_branch [= "-r|--remote|-a|--all"];`
    Branch { mode: BranchListMode },

    /// `pragma graft_json_branch [= "-r|--remote|-a|--all"];`
    JsonBranch { mode: BranchListMode },

    /// `pragma graft_branch_create = "name [start-point]";`
    BranchCreate {
        name: String,
        start_point: Option<String>,
    },

    /// `pragma graft_branch_delete = "[--force] name";`
    BranchDelete { name: String, force: bool },

    /// `pragma graft_branch_rename = "[--force] [old] new";`
    BranchRename {
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

    /// `pragma graft_branch_unset_upstream [= "branch"];`
    BranchUnsetUpstream { branch: Option<String> },

    /// `pragma graft_tag_create = "name [rev]";`
    /// `pragma graft_tag_create = "--annotated name [rev] -- message";`
    TagCreate {
        name: String,
        target: Option<String>,
        message: Option<String>,
    },

    /// `pragma graft_tag_delete = "name";`
    TagDelete { name: String },

    /// `pragma graft_switch_branch = "[--force] name";`
    SwitchBranch { name: String, force: bool },

    /// `pragma graft_switch_create = "[--force] name [start-point]";`
    SwitchCreate {
        name: String,
        start_point: Option<String>,
        force: bool,
    },

    /// `pragma graft_merge = "rev";`
    Merge { rev: String },

    /// `pragma graft_merge_abort;`
    MergeAbort,

    /// `pragma graft_merge_continue = "message";`
    MergeContinue { message: String },

    /// `pragma graft_conflicts;`
    Conflicts,

    /// `pragma graft_resolve = "--ours|--theirs [path]";`
    Resolve { spec: RepoResolveSpec },

    /// `pragma graft_remote_add = "name remote-uri";`
    RemoteAdd { name: String, config: RemoteConfig },

    /// `pragma graft_remote_remove = "name";`
    RemoteRemove { name: String },

    /// `pragma graft_remote_rename = "old new";`
    RemoteRename { old: String, new: String },

    /// `pragma graft_remote_get_url = "name";`
    RemoteGetUrl { name: String },

    /// `pragma graft_remote_set_url = "name remote-uri";`
    RemoteSetUrl { name: String, config: RemoteConfig },

    /// `pragma graft_remote_prune = "name";`
    RemotePrune { name: String },

    /// `pragma graft_ls_remote = "name";`
    LsRemote { name: String },

    /// `pragma graft_remotes;`
    Remotes,

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
    },

    /// `pragma graft_job_status = "job-id";`
    JobStatus { id: String },

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

    /// `pragma graft_diff = "[--staged] [rev] [rev] [-- path]";`
    /// Compare repository commits by revision syntax
    RepoDiff { spec: RepoDiffSpec },

    /// `pragma graft_show = "rev";`
    /// Display detailed info for specified revision
    Show { target: String },

    // JSON output variants (non-breaking additions)
    /// `pragma graft_json_log;`
    /// Repository commit history as JSON array
    JsonLog,

    /// `pragma graft_debug_volume_json_diff = "from_lsn,to_lsn[,mode]";`
    /// Legacy Volume diff as JSON. mode: omitted=summary, "rows"=row-level detail
    VolumeJsonDiff { from: LSN, to: LSN, mode: DiffMode },

    /// `pragma graft_json_diff = "[--staged] [rev] [rev] [-- path]";`
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
                "json_tags" => Ok(GraftPragma::JsonTags),
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
                "export" => {
                    let arg = p.require_arg()?;
                    let spec = parse_repo_export_arg(arg)?;
                    Ok(GraftPragma::Export { spec })
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
                "status" => Ok(GraftPragma::Status),
                "debug_volume_status" => Ok(GraftPragma::VolumeStatus),
                "init" => Ok(GraftPragma::RepoInit),
                "clone" => {
                    let spec = parse_repo_clone_arg(p.require_arg()?)?;
                    Ok(GraftPragma::RepoClone { spec })
                }
                "json_status" => Ok(GraftPragma::JsonStatus),
                "add" => Ok(GraftPragma::Add { path: p.arg.map(PathBuf::from) }),
                "rm" => Ok(GraftPragma::Remove { path: p.arg.map(PathBuf::from) }),
                "commit" => Ok(GraftPragma::Commit { message: p.require_arg()?.to_string() }),
                "branch" => Ok(GraftPragma::Branch { mode: parse_branch_list_mode(p.arg)? }),
                "json_branch" => {
                    Ok(GraftPragma::JsonBranch { mode: parse_branch_list_mode(p.arg)? })
                }
                "branch_create" => {
                    let (name, start_point) = parse_branch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchCreate { name, start_point })
                }
                "branch_delete" => {
                    let (name, force) = parse_branch_delete_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchDelete { name, force })
                }
                "branch_rename" => {
                    let (old, new, force) = parse_branch_rename_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchRename { old, new, force })
                }
                "branch_upstream" => {
                    let (branch, remote, remote_branch) =
                        parse_branch_upstream_arg(p.require_arg()?)?;
                    Ok(GraftPragma::BranchUpstream { branch, remote, remote_branch })
                }
                "branch_unset_upstream" => {
                    Ok(GraftPragma::BranchUnsetUpstream { branch: p.arg.map(str::to_string) })
                }
                "tag_create" => {
                    let (name, target, message) = parse_tag_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::TagCreate { name, target, message })
                }
                "tag_delete" => Ok(GraftPragma::TagDelete { name: p.require_arg()?.to_string() }),
                "switch_branch" => {
                    let (name, force) = parse_switch_branch_arg(p.require_arg()?)?;
                    Ok(GraftPragma::SwitchBranch { name, force })
                }
                "switch_create" => {
                    let (name, start_point, force) = parse_switch_create_arg(p.require_arg()?)?;
                    Ok(GraftPragma::SwitchCreate { name, start_point, force })
                }
                "merge" => Ok(GraftPragma::Merge { rev: p.require_arg()?.to_string() }),
                "merge_abort" => Ok(GraftPragma::MergeAbort),
                "merge_continue" => {
                    Ok(GraftPragma::MergeContinue { message: p.require_arg()?.to_string() })
                }
                "conflicts" => Ok(GraftPragma::Conflicts),
                "resolve" => Ok(GraftPragma::Resolve {
                    spec: parse_repo_resolve_arg(p.require_arg()?)?,
                }),
                "remote_add" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::RemoteAdd { name, config })
                }
                "remote_remove" => {
                    Ok(GraftPragma::RemoteRemove { name: p.require_arg()?.to_string() })
                }
                "remote_rename" => {
                    let (old, new) = parse_remote_rename(p.require_arg()?)?;
                    Ok(GraftPragma::RemoteRename { old, new })
                }
                "remote_get_url" => {
                    Ok(GraftPragma::RemoteGetUrl { name: p.require_arg()?.to_string() })
                }
                "remote_set_url" => {
                    let (name, config) = parse_remote_add(p.require_arg()?)?;
                    Ok(GraftPragma::RemoteSetUrl { name, config })
                }
                "remote_prune" => {
                    Ok(GraftPragma::RemotePrune { name: p.require_arg()?.to_string() })
                }
                "ls_remote" => Ok(GraftPragma::LsRemote { name: p.require_arg()?.to_string() }),
                "remotes" => Ok(GraftPragma::Remotes),
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
                    let arg = parse_remote_branch_arg(p.arg)?;
                    if arg.force {
                        return Err(pragma_fail("json_fetch_async does not support --force"));
                    }
                    let RemoteBranchArg { remote, branch, refspec, all, .. } = arg;
                    Ok(GraftPragma::JsonFetchAsync { remote, branch, refspec, all })
                }
                "job_status" => Ok(GraftPragma::JobStatus { id: p.require_arg()?.to_string() }),
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
                "json_log" => Ok(GraftPragma::JsonLog),
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
            GraftPragma::JsonTags => {
                let repo = repo_for_file(file)?;
                Ok(Some(to_json(&repo.tags()?)?))
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
                let restored = restore_repo_path(&runtime, file, &repo, &spec)?;
                Ok(Some(format!("Restored {restored}")))
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
            GraftPragma::Status => {
                let repo = repo_for_file(file)?;
                let status = repo_status_for_file(&runtime, file, &repo)?;
                Ok(Some(format_repo_status(&status)?))
            }
            GraftPragma::VolumeStatus => Ok(Some(format_volume_status(&runtime, file)?)),

            GraftPragma::RepoInit => {
                if !file.is_idle() {
                    return pragma_err!(
                        "cannot initialize a repository while there is an open transaction"
                    );
                }
                let repo = Repository::init_for_file(&file.tag)?;
                let preserved_contents = file.attach_repo_preserving_contents(repo.clone())?;
                if preserved_contents {
                    repo.mark_dirty_path(&file.tag)?;
                }
                Ok(Some(format!(
                    "Initialized empty Graft repository in {}",
                    repo.graft_dir().display()
                )))
            }

            GraftPragma::RepoClone { spec } => {
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
                    repo.remote_add("origin", spec.config)?;
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
                    repo.apply_switch_branch_plan(&branch, &plan)?;
                    checkout_repo_plan(
                        &runtime,
                        file,
                        &repo,
                        &plan,
                        &previous_files,
                        Some(remote),
                    )?;
                    Ok(Some(format!(
                        "Cloned origin/{} at {} into {}",
                        fetch.branch,
                        &fetch.head[..fetch.head.len().min(12)],
                        repo.graft_dir().display()
                    )))
                })();
                if result.is_err() && !attached {
                    let _ = std::fs::remove_dir_all(graft_dir);
                }
                result
            }

            GraftPragma::JsonStatus => {
                let repo = repo_for_file(file)?;
                let status = repo_status_for_file(&runtime, file, &repo)?;
                let conflict_analysis =
                    current_file_status_row_merge_analysis_lossy(&runtime, file, &repo, None);
                Ok(Some(
                    serde_json::to_string(&JsonRepoStatus { status, conflict_analysis })
                        .map_err(|e| ErrCtx::PragmaErr(format!("JSON error: {e}").into()))?,
                ))
            }

            GraftPragma::Add { path } => {
                if !file.is_idle() {
                    return pragma_err!("cannot add while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let entry = if let Some(path) = path.as_deref() {
                    let (key, physical_path) = repo_physical_path_arg(&repo, path)?;
                    if key == repo.file_key(&file.tag)? {
                        let state = current_repo_file_state(&runtime, file)?;
                        repo.stage_file_state_path(&file.tag, state)?
                    } else {
                        stage_physical_sqlite_file(&runtime, &repo, &key, &physical_path)?
                    }
                } else {
                    let state = current_repo_file_state(&runtime, file)?;
                    repo.stage_file_state_path(&file.tag, state)?
                };
                Ok(Some(format!("Added {}", entry.path)))
            }

            GraftPragma::Remove { path } => {
                if !file.is_idle() {
                    return pragma_err!("cannot remove while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let current_key = repo.file_key(&file.tag)?;
                let entry = if let Some(path) = path.as_deref() {
                    let (key, physical_path) = repo_physical_path_arg(&repo, path)?;
                    if key == current_key {
                        let entry = repo.stage_file_removal(&file.tag)?;
                        let volume = runtime.volume_open(None, None, None)?;
                        file.switch_volume(&volume.vid)?;
                        entry
                    } else {
                        remove_physical_sqlite_file(&repo, &key, &physical_path)?;
                        repo.stage_file_removal(&physical_path)?
                    }
                } else {
                    let entry = repo.stage_file_removal(&file.tag)?;
                    let volume = runtime.volume_open(None, None, None)?;
                    file.switch_volume(&volume.vid)?;
                    entry
                };
                Ok(Some(format!("Removed {}", entry.path)))
            }

            GraftPragma::Commit { message } => {
                if !file.is_idle() {
                    return pragma_err!("cannot commit while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let tables = staged_commit_table_summary(&runtime, &repo)?;
                let commit = repo.commit_staged_with_table_summary(message, tables)?;
                Ok(Some(format!("[{}] {}", &commit.id[..12], commit.message)))
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
                let branches = repo.branches()?;
                let remote_branches = if mode.includes_remote() {
                    repo.remote_tracking_branches()?
                } else {
                    Vec::new()
                };
                Ok(Some(to_json(&JsonBranchList {
                    branches,
                    remote_branches,
                })?))
            }

            GraftPragma::BranchCreate { name, start_point } => {
                let repo = repo_for_file(file)?;
                let branch = if start_point.is_some() || repo.status()?.head_target.is_some() {
                    repo.branch_create(&name, start_point.as_deref())?
                } else {
                    repo.branch_create_unborn(&name)?
                };
                Ok(Some(format_branch_created(&branch)))
            }

            GraftPragma::BranchDelete { name, force } => {
                let repo = repo_for_file(file)?;
                let branch = repo.branch_delete(&name, force)?;
                Ok(Some(format_branch_deleted(&branch, force)))
            }

            GraftPragma::BranchRename { old, new, force } => {
                let repo = repo_for_file(file)?;
                let old = match old {
                    Some(old) => old,
                    None => repo.current_branch()?.ok_or_else(|| {
                        ErrCtx::PragmaErr("cannot rename current branch in detached HEAD".into())
                    })?,
                };
                let branch = repo.branch_rename(&old, &new, force)?;
                Ok(Some(format_branch_renamed(&old, &branch, force)))
            }

            GraftPragma::BranchUpstream { branch, remote, remote_branch } => {
                let repo = repo_for_file(file)?;
                let branch = match branch {
                    Some(branch) => branch,
                    None => repo.current_branch()?.ok_or_else(|| {
                        ErrCtx::PragmaErr("cannot set upstream in detached HEAD".into())
                    })?,
                };
                let branch = repo.set_branch_upstream(&branch, &remote, &remote_branch)?;
                Ok(Some(format_branch_upstream(&branch)))
            }

            GraftPragma::BranchUnsetUpstream { branch } => {
                let repo = repo_for_file(file)?;
                let branch = match branch {
                    Some(branch) => branch,
                    None => repo.current_branch()?.ok_or_else(|| {
                        ErrCtx::PragmaErr("cannot unset upstream in detached HEAD".into())
                    })?,
                };
                let branch = repo.unset_branch_upstream(&branch)?;
                Ok(Some(format_branch_upstream_unset(&branch)))
            }

            GraftPragma::TagCreate { name, target, message } => {
                let repo = repo_for_file(file)?;
                let tag = match message {
                    Some(message) => {
                        repo.tag_create_annotated(&name, target.as_deref(), message)?
                    }
                    None => repo.tag_create(&name, target.as_deref())?,
                };
                Ok(Some(format_tag_created(&tag)))
            }

            GraftPragma::TagDelete { name } => {
                let repo = repo_for_file(file)?;
                let tag = repo.tag_delete(&name)?;
                Ok(Some(format_tag_deleted(&tag)))
            }

            GraftPragma::SwitchBranch { name, force } => {
                if !file.is_idle() {
                    return pragma_err!(
                        "cannot switch branches while there is an open transaction"
                    );
                }
                let repo = repo_for_file(file)?;
                let plan = repo.plan_switch_branch(&name)?;
                if repo_has_work_in_progress_for_file(&runtime, file, &repo)? {
                    if force {
                        repo.discard_work_in_progress()?;
                    } else {
                        return pragma_err!(
                            "cannot switch branches with staged or unstaged changes"
                        );
                    }
                }
                verify_repo_checkout_plan(&runtime, &plan, None)?;
                let previous_files = current_repo_files_for_checkout(&repo)?;
                repo.apply_switch_branch_plan(&name, &plan)?;
                checkout_repo_plan(&runtime, file, &repo, &plan, &previous_files, None)?;
                Ok(Some(format!("Switched to branch '{name}'")))
            }

            GraftPragma::SwitchCreate { name, start_point, force } => {
                if !file.is_idle() {
                    return pragma_err!(
                        "cannot switch branches while there is an open transaction"
                    );
                }
                let repo = repo_for_file(file)?;
                let plan = repo.plan_switch_new_branch(&name, start_point.as_deref())?;
                if repo_has_work_in_progress_for_file(&runtime, file, &repo)? {
                    if force {
                        repo.discard_work_in_progress()?;
                    } else {
                        return pragma_err!(
                            "cannot switch branches with staged or unstaged changes"
                        );
                    }
                }
                verify_repo_checkout_plan(&runtime, &plan.checkout, None)?;
                let previous_files = current_repo_files_for_checkout(&repo)?;
                let branch = repo.apply_switch_new_branch_plan(&plan)?;
                checkout_repo_plan(&runtime, file, &repo, &plan.checkout, &previous_files, None)?;
                Ok(Some(format_branch_created(&branch)))
            }

            GraftPragma::Merge { rev } => {
                if !file.is_idle() {
                    return pragma_err!("cannot merge while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                if repo_has_work_in_progress_for_file(&runtime, file, &repo)? {
                    return pragma_err!("cannot merge with staged or unstaged changes");
                }
                let plan = repo.plan_merge_revision(&rev)?;
                let plan = prepare_repo_merge_plan(&runtime, &plan, None)?;
                let previous_files = current_repo_files_for_checkout(&repo)?;
                let outcome = repo.apply_merge_plan(&plan)?;
                checkout_merge_outcome(
                    &runtime,
                    file,
                    &repo,
                    &outcome,
                    Some(&plan.checkout),
                    &previous_files,
                    None,
                )?;
                let row_auto_merge = match try_row_auto_merge_current_file_conflict(
                    &runtime, file, &repo, &outcome, None,
                ) {
                    Ok(row_auto_merge) => row_auto_merge,
                    Err(err) => {
                        tracing::warn!("row-level auto-merge unavailable: {err}");
                        None
                    }
                };
                Ok(Some(format_merge_outcome_with_row_auto_merge(
                    &runtime,
                    file,
                    &repo,
                    &outcome,
                    row_auto_merge.as_ref(),
                    None,
                )?))
            }

            GraftPragma::MergeAbort => {
                if !file.is_idle() {
                    return pragma_err!("cannot abort merge while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let plan = repo.plan_merge_abort()?;
                let previous_files = current_repo_files_for_checkout(&repo)?;
                let target = repo.apply_merge_abort_plan(&plan)?;
                checkout_repo_plan(&runtime, file, &repo, &plan.checkout, &previous_files, None)?;
                Ok(Some(format!(
                    "Aborted merge; reset HEAD to {}",
                    &target[..target.len().min(12)]
                )))
            }

            GraftPragma::MergeContinue { message } => {
                if !file.is_idle() {
                    return pragma_err!("cannot continue merge while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                if repo.status()?.merge_head.is_none() {
                    return pragma_err!("no merge in progress");
                }
                try_row_auto_merge_current_file_status_conflict(&runtime, file, &repo, None)?;
                let tables = staged_commit_table_summary(&runtime, &repo)?;
                let commit = repo.commit_staged_with_table_summary(message, tables)?;
                Ok(Some(format!(
                    "Merge commit [{}] {}",
                    &commit.id[..commit.id.len().min(12)],
                    commit.message
                )))
            }

            GraftPragma::Conflicts => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_conflicts(&repo.status()?)?))
            }

            GraftPragma::Resolve { spec } => {
                if !file.is_idle() {
                    return pragma_err!("cannot resolve while there is an open transaction");
                }
                let repo = repo_for_file(file)?;
                let path = spec.path.unwrap_or_else(|| PathBuf::from(&file.tag));
                let (key, physical_path) = repo_physical_path_arg(&repo, &path)?;
                let current_key = repo.file_key(&file.tag)?;
                let state = conflict_file_state(&repo, &physical_path, spec.side)?;
                if key == current_key {
                    if let Some(state) = &state {
                        checkout_repo_file_state(&runtime, file, state, None)?;
                    } else {
                        let volume = runtime.volume_open(None, None, None)?;
                        file.switch_volume(&volume.vid)?;
                    }
                } else if let Some(state) = &state {
                    checkout_repo_file_state_to_path(&runtime, &repo, state, &physical_path, None)?;
                } else {
                    remove_materialized_repo_file(&repo, &key)?;
                }
                let entry = repo.resolve_file_conflict(&physical_path, state)?;
                Ok(Some(format!(
                    "Resolved {} using {}",
                    entry.path,
                    spec.side.label()
                )))
            }

            GraftPragma::RemoteAdd { name, config } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_add(&name, config)?;
                Ok(Some(format_remote(&remote)))
            }

            GraftPragma::RemoteRemove { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_remove(&name)?;
                Ok(Some(format!("Removed remote '{}'", remote.name)))
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

            GraftPragma::RemoteGetUrl { name } => {
                let repo = repo_for_file(file)?;
                let remote = repo.remote_get_url(&name)?;
                Ok(Some(remote_config_uri(&remote.config)))
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

            GraftPragma::RemotePrune { name } => {
                let repo = repo_for_file(file)?;
                let outcome = repo.remote_prune(&name)?;
                Ok(Some(format_remote_prune_outcome(&outcome)?))
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

            GraftPragma::Remotes => {
                let repo = repo_for_file(file)?;
                Ok(Some(format_remotes(&repo.remotes()?)?))
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
            GraftPragma::JsonFetchAsync { remote, branch, refspec, all } => {
                repo_for_file(file)?;
                let id = async_jobs().spawn_fetch(
                    PathBuf::from(file.tag.clone()),
                    remote,
                    branch,
                    refspec,
                    all,
                    AsyncJobResultFormat::Json,
                );
                Ok(Some(id))
            }
            GraftPragma::JobStatus { id } => Ok(Some(async_jobs().status_json(&id)?)),
            GraftPragma::JobResult { id } => Ok(Some(async_jobs().result(&id)?)),
            GraftPragma::JsonJobResult { id } => Ok(Some(async_jobs().result(&id)?)),
            GraftPragma::Pull { remote, branch, refspec, all } => {
                let outcome = run_repo_pull(&runtime, file, remote, branch, refspec, all)?;
                let repo = repo_for_file(file)?;
                let checkout_remote = Arc::new(repo.remote_store(&outcome.remote)?);
                Ok(Some(format_pull_outcome_with_row_analysis(
                    &runtime,
                    file,
                    &repo,
                    &outcome,
                    Some(checkout_remote),
                )?))
            }
            GraftPragma::JsonPull { remote, branch, refspec, all } => {
                let outcome = run_repo_pull(&runtime, file, remote, branch, refspec, all)?;
                let repo = repo_for_file(file)?;
                let remote = repo.remote_store(&outcome.remote).ok().map(Arc::new);
                let conflict_analysis =
                    current_file_status_row_merge_analysis_lossy(&runtime, file, &repo, remote);
                Ok(Some(to_json(&JsonPullCommandOutcome {
                    operation: "pull",
                    outcome,
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
                Ok(Some(to_json(&outcome)?))
            }
            GraftPragma::VolumeFetch => Ok(Some(fetch_or_pull(&runtime, file, false)?)),
            GraftPragma::VolumePull => Ok(Some(fetch_or_pull(&runtime, file, true)?)),
            GraftPragma::VolumePush => Ok(Some(push(&runtime, file)?)),

            GraftPragma::VolumeAudit => Ok(Some(format_volume_audit(&runtime, file)?)),
            GraftPragma::VolumeJsonAudit => Ok(Some(to_json(&json_volume_audit(&runtime, file)?)?)),

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
                    &outcome.target[..outcome.target.len().min(12)],
                    reset_mode_label(mode)
                )))
            }
            GraftPragma::JsonReset { rev, mode } => {
                let outcome = run_repo_reset(&runtime, file, &rev, mode)?;
                Ok(Some(to_json(&JsonResetCommandOutcome {
                    operation: "reset",
                    outcome,
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

            GraftPragma::JsonLog => {
                let repo = repo_for_file(file)?;
                Ok(Some(serde_json::to_string(&repo.log()?).map_err(|e| {
                    ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                })?))
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
                let repo = repo_for_file(file)?;
                let diff = repo_diff_for_spec(&runtime, file, &repo, spec)?;
                match mode {
                    DiffMode::Default => {
                        Ok(Some(serde_json::to_string(&diff).map_err(|e| {
                            ErrCtx::PragmaErr(format!("JSON error: {e}").into())
                        })?))
                    }
                    DiffMode::Rows => {
                        let rows = json_repo_row_diff(&runtime, &repo, &diff)?;
                        Ok(Some(serde_json::to_string(&rows).map_err(|e| {
                            ErrCtx::PragmaErr(format!("JSON error: {e}").into())
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
                Ok(Some(serde_json::to_string(&commit).map_err(|e| {
                    ErrCtx::PragmaErr(format!("JSON error: {e}").into())
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
    to_json(&outcome)
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

fn run_repo_pull(
    runtime: &Runtime,
    file: &mut VolFile,
    remote: Option<String>,
    branch: Option<String>,
    refspec: Option<String>,
    all: bool,
) -> Result<PullOutcome, ErrCtx> {
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
    let previous_files = current_repo_files_for_checkout(&repo)?;
    let mut outcome = repo.apply_pull_plan(&plan)?;
    checkout_merge_outcome(
        runtime,
        file,
        &repo,
        &outcome.merge,
        Some(&plan.merge.checkout),
        &previous_files,
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
    Ok(outcome)
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
            verify_repo_checkout_plan(runtime, &plan, None)?;
            let previous_files = current_repo_files_for_checkout(&repo)?;
            let id = repo.apply_detach_plan(&rev, &plan)?;
            checkout_repo_plan(runtime, file, &repo, &plan, &previous_files, None)?;
            Ok(JsonCheckoutOutcome {
                operation: "checkout",
                target: id,
                path: None,
            })
        }
        RepoCheckoutSpec::Path { rev, path } => {
            let path = repo_path_arg(&repo, &path)?;
            let current_key = repo.file_key(&file.tag)?;
            let physical_path = repo.worktree().join(&path);
            let plan = repo.plan_checkout_file_key_from_revision(&rev, path)?;
            hydrate_repo_file_state(runtime, &plan.state, None)?;
            let outcome = repo.apply_checkout_file_plan(&plan)?;
            if outcome.path == current_key {
                checkout_repo_file_state(runtime, file, &outcome.state, None)?;
            } else {
                checkout_repo_file_state_to_path(
                    runtime,
                    &repo,
                    &outcome.state,
                    &physical_path,
                    None,
                )?;
            }
            Ok(JsonCheckoutOutcome {
                operation: "checkout",
                target: outcome.target,
                path: Some(outcome.path),
            })
        }
    }
}

fn format_checkout_outcome(outcome: &JsonCheckoutOutcome) -> String {
    match &outcome.path {
        Some(path) => format!(
            "Checked out {} from {}",
            path,
            &outcome.target[..outcome.target.len().min(12)]
        ),
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
) -> Result<ResetOutcome, ErrCtx> {
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
            checkout_repo_plan(runtime, file, &repo, &plan.checkout, &previous_files, None)?;
        }
    }

    Ok(outcome)
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
    for path in previous_files.keys() {
        if path == &key || plan.files.contains_key(path) {
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

fn remove_materialized_repo_file(repo: &Repository, key: &str) -> Result<(), ErrCtx> {
    let path = repo.worktree().join(key);
    match std::fs::symlink_metadata(&path) {
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

fn restore_repo_path(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    spec: &RepoRestoreSpec,
) -> Result<String, ErrCtx> {
    let (key, physical_path) = repo_physical_path_arg(repo, &spec.path)?;
    if spec.staged {
        if let Some(source) = &spec.source {
            repo.restore_index_path_from_revision(source, &physical_path)?;
        } else {
            repo.restore_index_path_from_head(&physical_path)?;
        }
        update_worktree_state_after_index_restore(runtime, file, repo, &physical_path)?;
        return Ok(key);
    }

    let restored = if let Some(source) = &spec.source {
        repo.file_from_revision(source, &physical_path)?
    } else {
        repo.index_file(&physical_path)?
    };

    if restored.is_none() {
        let can_restore_deletion = if spec.source.is_some() {
            repo.index_file(&physical_path)?.is_some()
                || repo.index_has_entry(&physical_path)?
                || repo.head_file(&physical_path)?.is_some()
        } else {
            repo.index_has_entry(&physical_path)?
        };
        if !can_restore_deletion {
            return Err(ErrCtx::PragmaErr(
                format!("path `{key}` is not tracked").into(),
            ));
        }
    }

    let current_key = repo.file_key(&file.tag)?;
    if key == current_key {
        if let Some(state) = &restored {
            checkout_repo_file_state(runtime, file, state, None)?;
        } else {
            let volume = runtime.volume_open(None, None, None)?;
            file.switch_volume(&volume.vid)?;
        }
    } else if let Some(state) = &restored {
        checkout_repo_file_state_to_path(runtime, repo, state, &physical_path, None)?;
    } else {
        remove_materialized_repo_file(repo, &key)?;
    }

    update_restored_worktree_state(runtime, repo, &physical_path, restored.as_ref())?;
    Ok(key)
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

fn update_worktree_state_after_index_restore(
    runtime: &Runtime,
    file: &VolFile,
    repo: &Repository,
    path: &Path,
) -> Result<(), ErrCtx> {
    if repo.file_key(path)? != repo.file_key(&file.tag)? {
        repo.clear_dirty_path(path)?;
        return Ok(());
    }

    let worktree_state = current_repo_file_state(runtime, file)?;
    let index_state = repo.index_file(path)?;
    let matches_index = match index_state.as_ref() {
        Some(index_state) => repo_file_state_content_eq(runtime, &worktree_state, index_state)?,
        None => false,
    };
    if matches_index {
        repo.clear_dirty_path(path)?;
    } else {
        repo.mark_dirty_path(path)?;
    }
    Ok(())
}

fn update_restored_worktree_state(
    runtime: &Runtime,
    repo: &Repository,
    path: &Path,
    restored: Option<&CommitFileState>,
) -> Result<(), ErrCtx> {
    let index_state = repo.index_file(path)?;
    let matches_index = match (restored, index_state.as_ref()) {
        (Some(restored), Some(index_state)) => {
            repo_file_state_content_eq(runtime, restored, index_state)?
        }
        (None, None) => true,
        _ => false,
    };

    if matches_index {
        repo.clear_dirty_path(path)?;
    } else if restored.is_none() {
        repo.mark_deleted_path(path)?;
    } else {
        repo.mark_dirty_path(path)?;
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
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    match outcome {
        MergeOutcome::FastForward { .. } => {
            if let Some(plan) = fast_forward_plan {
                checkout_repo_plan(runtime, file, repo, plan, previous_files, remote)?;
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

fn hydrate_repo_snapshot(
    runtime: &Runtime,
    snapshot: &RepoSnapshot,
    remote: Option<Arc<Remote>>,
) -> Result<(), ErrCtx> {
    RepoSnapshotResolver::strict(runtime, remote, RepoSnapshotPurpose::Push)
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
            hydrate_repo_snapshot(runtime, &snapshot, None)?;
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
    } else {
        return Err(pragma_fail(
            "remote URI must start with memory, fs://, s3://, or s3_compatible://",
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
            target: RepoDiffTarget::Worktree { path: None },
        });
    };
    let raw_parts: Vec<&str> = arg.split_whitespace().collect();
    let mut mode = DiffMode::Default;
    let mut parts = Vec::new();
    let mut in_path = false;
    for part in raw_parts {
        if !in_path && part == "--" {
            in_path = true;
            parts.push(part);
        } else if !in_path && part == "--rows" {
            if mode == DiffMode::Rows {
                return Err(pragma_fail("`--rows` may only be specified once"));
            }
            mode = DiffMode::Rows;
        } else {
            parts.push(part);
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
    Ok(RepoDiffSpec { mode, target })
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
    let parts: Vec<&str> = arg.split_whitespace().collect();
    match parts.as_slice() {
        [
            "--staged" | "--cached",
            "--source" | "-s",
            source,
            "--",
            path @ ..,
        ]
        | [
            "--source" | "-s",
            source,
            "--staged" | "--cached",
            "--",
            path @ ..,
        ] if !path.is_empty() => Ok(RepoRestoreSpec {
            source: Some((*source).to_string()),
            staged: true,
            path: PathBuf::from(path.join(" ")),
        }),
        [
            "--staged" | "--cached",
            "--source" | "-s",
            source,
            path @ ..,
        ]
        | [
            "--source" | "-s",
            source,
            "--staged" | "--cached",
            path @ ..,
        ] if !path.is_empty() => Ok(RepoRestoreSpec {
            source: Some((*source).to_string()),
            staged: true,
            path: PathBuf::from(path.join(" ")),
        }),
        ["--staged" | "--cached", "--", path @ ..] if !path.is_empty() => Ok(RepoRestoreSpec {
            source: None,
            staged: true,
            path: PathBuf::from(path.join(" ")),
        }),
        ["--staged" | "--cached", path @ ..] if !path.is_empty() => Ok(RepoRestoreSpec {
            source: None,
            staged: true,
            path: PathBuf::from(path.join(" ")),
        }),
        ["--source" | "-s", source, "--", path @ ..] if !path.is_empty() => Ok(RepoRestoreSpec {
            source: Some((*source).to_string()),
            staged: false,
            path: PathBuf::from(path.join(" ")),
        }),
        ["--source" | "-s", source, path @ ..] if !path.is_empty() => Ok(RepoRestoreSpec {
            source: Some((*source).to_string()),
            staged: false,
            path: PathBuf::from(path.join(" ")),
        }),
        ["--", path @ ..] if !path.is_empty() => Ok(RepoRestoreSpec {
            source: None,
            staged: false,
            path: PathBuf::from(path.join(" ")),
        }),
        path @ [_first, ..] => Ok(RepoRestoreSpec {
            source: None,
            staged: false,
            path: PathBuf::from(path.join(" ")),
        }),
        _ => Err(pragma_fail(
            "argument must be in the form: `[--staged] [--source rev] path`",
        )),
    }
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
    let mut path = Vec::new();

    for part in arg.split_whitespace() {
        match part {
            "--ours" => {
                if side.replace(ResolveSide::Ours).is_some() {
                    return Err(pragma_fail("resolve accepts only one side"));
                }
            }
            "--theirs" => {
                if side.replace(ResolveSide::Theirs).is_some() {
                    return Err(pragma_fail("resolve accepts only one side"));
                }
            }
            value => path.push(value),
        }
    }

    let Some(side) = side else {
        return Err(pragma_fail("argument must include `--ours` or `--theirs`"));
    };

    Ok(RepoResolveSpec {
        side,
        path: (!path.is_empty()).then(|| PathBuf::from(path.join(" "))),
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
        return Ok(repo.file_key(path_obj)?);
    }
    Ok(path.trim_start_matches("./").replace('\\', "/"))
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
    match spec.target {
        RepoDiffTarget::Worktree { path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            let current_key = repo.file_key(&file.tag)?;
            if let Some(path) = path.as_deref()
                && path != current_key
            {
                let (key, physical_path, state) =
                    physical_worktree_file_state(runtime, repo, path)?;
                let expected = repo.index_files()?.get(&key).cloned();
                return match state {
                    Some(state) => {
                        let state = if let Some(expected) = expected
                            && repo_file_state_content_eq(runtime, &state, &expected)?
                        {
                            expected
                        } else {
                            state
                        };
                        Ok(repo.diff_worktree_file(&physical_path, state, Some(&key))?)
                    }
                    None => Ok(repo.diff_worktree_file_removal(&physical_path, Some(&key))?),
                };
            }
            let state = current_repo_file_state(runtime, file)?;
            Ok(repo.diff_worktree_file(&file.tag, state, path.as_deref())?)
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
                let (key, physical_path, state) =
                    physical_worktree_file_state(runtime, repo, path)?;
                let from_id = repo.resolve_revision(&rev)?;
                let expected = repo.read_commit(&from_id)?.files.get(&key).cloned();
                return match state {
                    Some(state) => {
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
                    None => Ok(repo.diff_revision_to_worktree_file_removal(
                        &rev,
                        &physical_path,
                        Some(&key),
                    )?),
                };
            }
            let state = current_repo_file_state(runtime, file)?;
            Ok(repo.diff_revision_to_worktree_file(&rev, &file.tag, state, path.as_deref())?)
        }
        RepoDiffTarget::Revisions { from, to, path } => {
            let path = repo_diff_path(repo, path.as_deref())?;
            Ok(repo.diff_revisions(&from, &to, path.as_deref())?)
        }
    }
}

fn physical_worktree_file_state(
    runtime: &Runtime,
    repo: &Repository,
    path: &str,
) -> Result<(String, PathBuf, Option<CommitFileState>), ErrCtx> {
    let (key, physical_path) = repo_physical_path_arg(repo, Path::new(path))?;
    match std::fs::symlink_metadata(&physical_path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(ErrCtx::PragmaErr(
                    format!(
                        "path `{}` is not a regular SQLite database file",
                        physical_path.display()
                    )
                    .into(),
                ));
            }
            let state = import_physical_sqlite_file_state(runtime, &physical_path)?;
            Ok((key, physical_path, Some(state)))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok((key, physical_path, None)),
        Err(err) => Err(err.into()),
    }
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
    status.unstaged_changes.retain(|change| {
        change.path == current_key
            || tracked.contains_key(&change.path)
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
                .push(graft::repo::RepoWorktreeChange { path: key, change });
        }
    }
    status.unstaged_changes.sort_by(|a, b| a.path.cmp(&b.path));
    status.unstaged = status
        .unstaged_changes
        .iter()
        .map(|change| change.path.clone())
        .collect();
    status.dirty = !status.unstaged_changes.is_empty();
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
    Ok(status.dirty
        || !status.staged.is_empty()
        || !status.conflicted.is_empty()
        || status.merge_head.is_some())
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
    let mut file = File::open(path)?;
    let mut magic = [0_u8; SQLITE_DATABASE_MAGIC.len()];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == SQLITE_DATABASE_MAGIC),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err.into()),
    }
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

fn conflict_file_state(
    repo: &Repository,
    path: &Path,
    side: ResolveSide,
) -> Result<Option<CommitFileState>, ErrCtx> {
    let key = repo.file_key(path)?;
    let stage = side.index_stage();
    let index = repo.read_index()?;
    index
        .entries
        .iter()
        .find(|entry| entry.path == key && entry.stage == stage)
        .map(|entry| entry.file.clone())
        .ok_or_else(|| {
            ErrCtx::PragmaErr(format!("path `{key}` has no {} conflict stage", side.label()).into())
        })
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
    if diff.files.is_empty() {
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
        "Resolve a path with `pragma graft_resolve = \"--ours [path]\"` or `pragma graft_resolve = \"--theirs [path]\"`."
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
              unavailable: merge involves add/delete of this database path.
            "
        )));
    };

    hydrate_repo_file_state_for(runtime, base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, theirs, remote, RepoSnapshotPurpose::Merge)?;
    let analysis = crate::row_merge::analyze_snapshot_merge(runtime, base, ours, theirs)?;
    let mut f = String::new();
    writeln!(&mut f, "Row-level analysis for {key}:")?;
    writeln!(&mut f, "  ours: {} row change(s)", analysis.ours_changes)?;
    writeln!(
        &mut f,
        "  theirs: {} row change(s)",
        analysis.theirs_changes
    )?;
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
    } else {
        writeln!(
            &mut f,
            "  No row conflicts detected; row-level auto-merge candidate."
        )?;
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
            blocked_reasons: vec!["add_delete_conflict"],
            row_conflicts: vec![],
            schema_conflicts: vec![],
            message: Some("merge involves add/delete of this database path".to_string()),
        }));
    };

    hydrate_repo_file_state_for(runtime, &base, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &ours, None, RepoSnapshotPurpose::Merge)?;
    hydrate_repo_file_state_for(runtime, &theirs, remote, RepoSnapshotPurpose::Merge)?;

    let plan = crate::row_merge::plan_snapshot_merge(runtime, &base, &ours, &theirs)?;
    let row_conflicts: Vec<JsonRowMergeConflict> = plan
        .analysis
        .conflicts
        .iter()
        .map(|conflict| JsonRowMergeConflict {
            table: conflict.table.clone(),
            rowid: conflict.rowid,
            ours: row_change_kind_label(conflict.ours),
            theirs: row_change_kind_label(conflict.theirs),
        })
        .collect();
    let schema_conflicts: Vec<JsonSchemaMergeConflict> = plan
        .schema_conflicts()
        .iter()
        .map(|conflict| JsonSchemaMergeConflict {
            name: conflict.name.clone(),
            entry_type: conflict.entry_type.clone(),
            ours: conflict.ours.map(schema_change_kind_label),
            theirs: conflict.theirs.map(schema_change_kind_label),
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
        blocked_reasons,
        row_conflicts,
        schema_conflicts,
        message: None,
    }))
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

    try_row_auto_merge_current_file_status_conflict(runtime, file, repo, remote)
}

fn try_row_auto_merge_current_file_status_conflict(
    runtime: &Runtime,
    file: &mut VolFile,
    repo: &Repository,
    remote: Option<Arc<Remote>>,
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

    let plan = crate::row_merge::plan_snapshot_merge(runtime, &base, &ours, &theirs)?;
    if plan.has_conflicts() || plan.has_opaque_changes() || plan.apply_change_count() == 0 {
        return Ok(None);
    }

    let applied_changes = plan.apply_change_count();
    let sql = plan.theirs_apply_sql();
    let merged = materialize_row_auto_merge_state(runtime, repo, &key, &ours, &sql)?;
    checkout_repo_file_state(runtime, file, &merged, None)?;
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

fn json_repo_row_diff(
    runtime: &Runtime,
    repo: &Repository,
    diff: &RepoDiff,
) -> Result<crate::json::JsonRepoRowDiffResult, ErrCtx> {
    let files = diff
        .files
        .iter()
        .map(|file| {
            let change = repo_file_change_label(file.change).to_string();
            match repo_file_row_diff(runtime, repo, file) {
                Ok(Some(row_diff)) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    change,
                    row_diff_available: true,
                    message: None,
                    tables: json_table_changes(&row_diff.table_changes),
                    opaque_changes: json_opaque_changes(&row_diff.opaque_changes),
                }),
                Ok(None) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    change: change.clone(),
                    row_diff_available: false,
                    message: Some(format!(
                        "row diff unavailable for {change} database snapshots"
                    )),
                    tables: Vec::new(),
                    opaque_changes: Vec::new(),
                }),
                Err(err) => Ok(crate::json::JsonRepoRowDiffFile {
                    path: file.path.clone(),
                    change: change.clone(),
                    row_diff_available: false,
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
            GraftPragma::Status
        ));

        let remove = Pragma { name: "graft_rm", arg: Some("app.db") };
        assert!(matches!(
            GraftPragma::try_from(&remove).unwrap(),
            GraftPragma::Remove { .. }
        ));
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

        let status = Pragma {
            name: "graft_job_status",
            arg: Some("graft-job-1"),
        };
        assert!(matches!(
            GraftPragma::try_from(&status).unwrap(),
            GraftPragma::JobStatus { .. }
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
    fn parse_remote_rename_requires_two_names() {
        assert_eq!(
            parse_remote_rename("origin upstream").unwrap(),
            ("origin".to_string(), "upstream".to_string())
        );
        assert!(parse_remote_rename("origin").is_err());
        assert!(parse_remote_rename("origin upstream backup").is_err());
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
                target: RepoDiffTarget::Worktree { path: None },
            }
        );
        assert_eq!(
            parse_repo_diff_arg(Some("--rows --staged -- app.db")).unwrap(),
            RepoDiffSpec {
                mode: DiffMode::Rows,
                target: RepoDiffTarget::Staged { path: Some("app.db".to_string()) },
            }
        );
        assert_eq!(
            parse_repo_diff_arg(Some("--rows HEAD~1 HEAD -- app.db")).unwrap(),
            RepoDiffSpec {
                mode: DiffMode::Rows,
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
                target: RepoDiffTarget::RevisionToWorktree {
                    rev: "HEAD".to_string(),
                    path: Some("--rows".to_string()),
                },
            }
        );
        assert!(parse_repo_diff_arg(Some("--rows --rows")).is_err());
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
                path: PathBuf::from("external.db"),
            }
        );
        assert_eq!(
            parse_repo_restore_arg("--source HEAD~1 -- external.db").unwrap(),
            RepoRestoreSpec {
                source: Some("HEAD~1".to_string()),
                staged: false,
                path: PathBuf::from("external.db"),
            }
        );
        assert_eq!(
            parse_repo_restore_arg("--staged -- external.db").unwrap(),
            RepoRestoreSpec {
                source: None,
                staged: true,
                path: PathBuf::from("external.db"),
            }
        );
        assert_eq!(
            parse_repo_restore_arg("--staged --source HEAD -- external.db").unwrap(),
            RepoRestoreSpec {
                source: Some("HEAD".to_string()),
                staged: true,
                path: PathBuf::from("external.db"),
            }
        );
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
    }
}
