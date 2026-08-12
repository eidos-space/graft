use std::{path::PathBuf, sync::Arc};

use graft_sdk::{
    AbortMergeOptions as CoreAbortMergeOptions, ApplyMergeOptions as CoreApplyMergeOptions,
    CancellationToken, CommitChangedPathsOptions as CoreCommitChangedPathsOptions,
    ContinueMergeOptions as CoreContinueMergeOptions,
    DiffMergeSqliteOptions as CoreDiffMergeSqliteOptions, DiffOptions as CoreDiffOptions,
    DiffPathsOptions as CoreDiffPathsOptions, IgnoredPathsOptions as CoreIgnoredPathsOptions,
    InventoryKind, InventoryOptions as CoreInventoryOptions,
    ListMergeConflictsOptions as CoreListMergeConflictsOptions,
    ListMergePathsOptions as CoreListMergePathsOptions, MergePathFilter as CoreMergePathFilter,
    MergePathResult as CoreMergePathResult, MergePolicyDocument as CoreMergePolicyDocument,
    MergeSqliteVersion as CoreMergeSqliteVersion, MergeVersion as CoreMergeVersion,
    PlanMergeOptions as CorePlanMergeOptions,
    ReadMergeVersionOptions as CoreReadMergeVersionOptions,
    ReadPathContentOptions as CoreReadPathContentOptions,
    RecordPathMoveOptions as CoreRecordPathMoveOptions,
    RemoteConfigureOptions as CoreRemoteConfigureOptions, RepositoryOperation,
    RepositorySession as CoreRepositorySession,
    ResolveMergeCellOptions as CoreResolveMergeCellOptions,
    ResolveMergeRowOptions as CoreResolveMergeRowOptions,
    ResolveMergeTableOptions as CoreResolveMergeTableOptions, RestoreOptions as CoreRestoreOptions,
    RestorePathsOptions as CoreRestorePathsOptions, SdkError,
    SetMergePathResultOptions as CoreSetMergePathResultOptions,
    SetMergePolicyOptions as CoreSetMergePolicyOptions,
    SqliteDiffPathsOptions as CoreSqliteDiffPathsOptions, SqliteDiffResponse,
    StageMergeSqliteResultOptions as CoreStageMergeSqliteResultOptions,
    StagePathsOptions as CoreStagePathsOptions,
    UnresolveMergePathOptions as CoreUnresolveMergePathOptions,
    UntrackPathsOptions as CoreUntrackPathsOptions,
    WriteAndStageTextResultOptions as CoreWriteAndStageTextResultOptions,
};
use napi::{
    Env, Error, Result, Status, Task,
    bindgen_prelude::{AbortSignal, AsyncTask},
};
use napi_derive::napi;

#[napi(object)]
pub struct DiffOptions {
    pub rows: Option<bool>,
    pub staged: Option<bool>,
    pub root: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub path: Option<String>,
    pub table: Option<String>,
}

#[napi(object)]
pub struct DiffPathsOptions {
    pub paths: Vec<String>,
    pub rows: Option<bool>,
    pub root: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub table: Option<String>,
    pub limit: Option<u32>,
    pub after: Option<String>,
}

#[napi(object)]
pub struct SqliteDiffPathsOptions {
    pub paths: Vec<String>,
    pub mode: String,
    pub staged: Option<bool>,
    pub staged_fallback: Option<bool>,
    pub root: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub table: Option<String>,
    pub row_limit: Option<u32>,
    pub row_after: Option<String>,
    pub limit: Option<u32>,
    pub after: Option<String>,
}

#[napi(object)]
pub struct ReadPathContentOptions {
    pub path: String,
    pub revision: String,
    pub max_bytes: u32,
}

#[napi(object)]
pub struct CommitChangedPathsOptions {
    pub revision: String,
    pub limit: Option<u32>,
    pub after: Option<String>,
}

#[napi(object)]
pub struct RestoreOptions {
    pub source: Option<String>,
    pub expected_head: Option<String>,
    pub require_clean: Option<bool>,
    pub path: String,
}

#[napi(object)]
pub struct StagePathsOptions {
    pub paths: Vec<String>,
    pub expected_head: Option<String>,
    pub force: Option<bool>,
}

#[napi(object)]
pub struct RecordPathMoveOptions {
    pub previous_path: String,
    pub path: String,
    pub expected_head: Option<String>,
}

#[napi(object)]
pub struct UntrackPathsOptions {
    pub paths: Vec<String>,
    pub expected_head: Option<String>,
}

#[napi(object)]
pub struct RestorePathsOptions {
    pub source: Option<String>,
    pub expected_head: Option<String>,
    pub require_clean: Option<bool>,
    pub paths: Vec<String>,
}

#[napi(object)]
pub struct InventoryOptions {
    pub kind: Option<String>,
    pub limit: Option<u32>,
    pub after: Option<String>,
}

#[napi(object)]
pub struct IgnoredPathsOptions {
    pub paths: Vec<String>,
}

#[napi(object)]
pub struct RemoteConfigureOptions {
    pub name: String,
    pub url: String,
    pub bearer_token: Option<String>,
    pub overwrite: Option<bool>,
    pub upstream_branch: Option<String>,
}

#[napi(object)]
pub struct CloneOptions {
    pub remote_url: String,
    pub branch: Option<String>,
    pub bearer_token: Option<String>,
}

#[napi(object)]
pub struct PlanMergeOptions {
    pub revision: String,
    pub expected_head: Option<String>,
}

#[napi(object)]
pub struct ApplyMergeOptions {
    pub revision: String,
    pub expected_head: Option<String>,
    pub plan_token: String,
}

#[napi(object)]
pub struct SetMergePolicyOptions {
    pub policy_json: String,
    pub expected_policy_token: String,
}

#[napi(object)]
pub struct ListMergePathsOptions {
    pub filter: String,
    pub limit: u32,
    pub after: Option<String>,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct ListMergeConflictsOptions {
    pub path: String,
    pub limit: u32,
    pub after: Option<String>,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct ReadMergeVersionOptions {
    pub path: String,
    pub version: String,
    pub max_bytes: u32,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct DiffMergeSqliteOptions {
    pub path: String,
    pub from: String,
    pub to: String,
    pub mode: String,
    pub table: Option<String>,
    pub row_limit: Option<u32>,
    pub row_after: Option<String>,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct SetMergePathResultOptions {
    pub path: String,
    pub result: String,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct ResolveMergeRowOptions {
    pub path: String,
    pub table: String,
    pub identity: String,
    pub result: String,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct ResolveMergeCellOptions {
    pub path: String,
    pub table: String,
    pub identity: String,
    pub column: String,
    pub result: String,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct ResolveMergeTableOptions {
    pub path: String,
    pub table: String,
    pub result: String,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct UnresolveMergePathOptions {
    pub path: String,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct StageMergeSqliteResultOptions {
    pub path: String,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct WriteAndStageTextResultOptions {
    pub path: String,
    pub content: String,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct ContinueMergeOptions {
    pub message: String,
    pub expected_state_token: String,
}

#[napi(object)]
pub struct AbortMergeOptions {
    pub expected_state_token: String,
}

enum JsonOperation {
    Init,
    Status,
    StatusIncremental,
    RepositoryMetadata,
    ListRemotes,
    AddAll,
    StagePaths {
        options: CoreStagePathsOptions,
    },
    RecordPathMove {
        options: CoreRecordPathMoveOptions,
    },
    UntrackPaths {
        options: CoreUntrackPathsOptions,
    },
    Commit {
        message: String,
    },
    Diff {
        options: CoreDiffOptions,
    },
    DiffPaths {
        options: CoreDiffPathsOptions,
    },
    SqliteDiffPaths {
        options: CoreSqliteDiffPathsOptions,
    },
    ReadPathContent {
        options: CoreReadPathContentOptions,
    },
    History {
        limit: usize,
        after: Option<String>,
    },
    HistorySummaries {
        limit: usize,
        after: Option<String>,
    },
    CommitDetails {
        revision: String,
    },
    CommitChangedPaths {
        options: CoreCommitChangedPathsOptions,
    },
    IsIgnoredPath {
        path: PathBuf,
    },
    IsIgnoredPaths {
        options: CoreIgnoredPathsOptions,
    },
    Inventory {
        options: CoreInventoryOptions,
    },
    Restore {
        options: CoreRestoreOptions,
    },
    RestorePaths {
        options: CoreRestorePathsOptions,
    },
    ConfigureRemote {
        options: CoreRemoteConfigureOptions,
    },
    Push {
        remote: Option<String>,
        branch: Option<String>,
    },
    Fetch {
        remote: Option<String>,
        branch: Option<String>,
    },
    Pull {
        remote: Option<String>,
        branch: Option<String>,
    },
    Clone {
        remote_url: String,
        branch: Option<String>,
        bearer_token: Option<String>,
    },
    PlanMerge {
        options: CorePlanMergeOptions,
    },
    GetMergePolicy,
    ValidateMergePolicy {
        policy_json: String,
    },
    SetMergePolicy {
        policy_json: String,
        expected_policy_token: String,
    },
    ApplyMerge {
        options: CoreApplyMergeOptions,
    },
    GetMergeStatus,
    ListMergePaths {
        options: CoreListMergePathsOptions,
    },
    ListMergeConflicts {
        options: CoreListMergeConflictsOptions,
    },
    ReadMergeVersion {
        options: CoreReadMergeVersionOptions,
    },
    DiffMergeSqlite {
        options: CoreDiffMergeSqliteOptions,
    },
    SetMergePathResult {
        options: CoreSetMergePathResultOptions,
    },
    ResolveMergeRow {
        options: CoreResolveMergeRowOptions,
    },
    ResolveMergeCell {
        options: CoreResolveMergeCellOptions,
    },
    ResolveMergeTable {
        options: CoreResolveMergeTableOptions,
    },
    StageMergeSqliteResult {
        options: CoreStageMergeSqliteResultOptions,
    },
    UnresolveMergePath {
        options: CoreUnresolveMergePathOptions,
    },
    WriteAndStageTextResult {
        options: CoreWriteAndStageTextResultOptions,
    },
    ContinueMerge {
        options: CoreContinueMergeOptions,
    },
    AbortMerge {
        options: CoreAbortMergeOptions,
    },
}

pub struct JsonTask {
    session: Arc<CoreRepositorySession>,
    operation: JsonOperation,
    cancellation: CancellationToken,
}

impl Task for JsonTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        let cancellation = self.cancellation.clone();
        graft_sdk::with_cancellation(&cancellation, || self.compute_inner())
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

impl JsonTask {
    fn compute_inner(&mut self) -> Result<String> {
        if matches!(self.operation, JsonOperation::StatusIncremental) {
            let value = self.session.status_incremental().map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if matches!(self.operation, JsonOperation::RepositoryMetadata) {
            let value = self.session.repository_metadata().map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if matches!(self.operation, JsonOperation::ListRemotes) {
            let value = self.session.list_remotes().map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::HistorySummaries { limit, after } = &self.operation {
            let value = self
                .session
                .history_summaries(*limit, after.as_deref())
                .map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::DiffPaths { options } = &self.operation {
            let value = self.session.diff_paths(options).map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::SqliteDiffPaths { options } = &self.operation {
            let value = self
                .session
                .diff_sqlite_paths(options)
                .map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::ReadPathContent { options } = &self.operation {
            let value = self
                .session
                .read_path_content(options)
                .map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::CommitChangedPaths { options } = &self.operation {
            let value = self
                .session
                .commit_changed_paths(options)
                .map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::StagePaths { options } = &self.operation {
            let value = self.session.stage_paths(options).map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::RecordPathMove { options } = &self.operation {
            let value = self.session.record_path_move(options).map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::UntrackPaths { options } = &self.operation {
            let value = self.session.untrack_paths(options).map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::RestorePaths { options } = &self.operation {
            let value = self.session.restore_paths(options).map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::IsIgnoredPath { path } = &self.operation {
            let value = self.session.is_ignored_path(path).map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::IsIgnoredPaths { options } = &self.operation {
            let value = self.session.is_ignored_paths(options).map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        if let JsonOperation::Inventory { options } = &self.operation {
            let value = self.session.inventory(options).map_err(napi_error)?;
            return serde_json::to_string(&value)
                .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
        }
        match &self.operation {
            JsonOperation::GetMergePolicy => {
                let value = self.session.get_merge_policy().map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::ValidateMergePolicy { policy_json } => {
                let policy = serde_json::from_str::<CoreMergePolicyDocument>(policy_json)
                    .map_err(|error| Error::new(Status::InvalidArg, error.to_string()))?;
                let value = self.session.validate_merge_policy(&policy);
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::SetMergePolicy { policy_json, expected_policy_token } => {
                let policy = serde_json::from_str::<CoreMergePolicyDocument>(policy_json)
                    .map_err(|error| Error::new(Status::InvalidArg, error.to_string()))?;
                let value = self
                    .session
                    .set_merge_policy(&CoreSetMergePolicyOptions {
                        policy,
                        expected_policy_token: expected_policy_token.clone(),
                    })
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::PlanMerge { options } => {
                let value = self.session.plan_merge(options).map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::ApplyMerge { options } => {
                let value = self.session.apply_merge(options).map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::GetMergeStatus => {
                let value = self.session.get_merge_status().map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::ListMergePaths { options } => {
                let value = self.session.list_merge_paths(options).map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::ListMergeConflicts { options } => {
                let value = self
                    .session
                    .list_merge_conflicts(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::ReadMergeVersion { options } => {
                let value = self
                    .session
                    .read_merge_version(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::DiffMergeSqlite { options } => {
                let value = self
                    .session
                    .diff_merge_sqlite(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::SetMergePathResult { options } => {
                let value = self
                    .session
                    .set_merge_path_result(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::ResolveMergeRow { options } => {
                let value = self
                    .session
                    .resolve_merge_row(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::ResolveMergeCell { options } => {
                let value = self
                    .session
                    .resolve_merge_cell(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::ResolveMergeTable { options } => {
                let value = self
                    .session
                    .resolve_merge_table(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::StageMergeSqliteResult { options } => {
                let value = self
                    .session
                    .stage_merge_sqlite_result(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::UnresolveMergePath { options } => {
                let value = self
                    .session
                    .unresolve_merge_path(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::WriteAndStageTextResult { options } => {
                let value = self
                    .session
                    .write_and_stage_text_result(options)
                    .map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::ContinueMerge { options } => {
                let value = self.session.continue_merge(options).map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            JsonOperation::AbortMerge { options } => {
                let value = self.session.abort_merge(options).map_err(napi_error)?;
                return serde_json::to_string(&value)
                    .map_err(|error| Error::new(Status::GenericFailure, error.to_string()));
            }
            _ => {}
        }
        let value = match &mut self.operation {
            JsonOperation::Init => self.session.init(),
            JsonOperation::Status => self.session.status(),
            JsonOperation::StatusIncremental => unreachable!("handled before JSON value dispatch"),
            JsonOperation::RepositoryMetadata | JsonOperation::ListRemotes => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::AddAll => self.session.add_all(),
            JsonOperation::StagePaths { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::RecordPathMove { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::UntrackPaths { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::Commit { message } => self.session.commit(message),
            JsonOperation::Diff { options } => self.session.diff(options),
            JsonOperation::DiffPaths { .. } | JsonOperation::SqliteDiffPaths { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::ReadPathContent { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::History { limit, after } => {
                self.session.history(*limit, after.as_deref())
            }
            JsonOperation::HistorySummaries { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::CommitDetails { revision } => self.session.commit_details(revision),
            JsonOperation::CommitChangedPaths { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::IsIgnoredPath { .. }
            | JsonOperation::IsIgnoredPaths { .. }
            | JsonOperation::Inventory { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::Restore { options } => self.session.restore(options),
            JsonOperation::RestorePaths { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
            JsonOperation::ConfigureRemote { options } => self.session.configure_remote(options),
            JsonOperation::Push { remote, branch } => {
                self.session.push(remote.as_deref(), branch.as_deref())
            }
            JsonOperation::Fetch { remote, branch } => {
                self.session.fetch(remote.as_deref(), branch.as_deref())
            }
            JsonOperation::Pull { remote, branch } => {
                self.session.pull(remote.as_deref(), branch.as_deref())
            }
            JsonOperation::Clone { remote_url, branch, bearer_token } => self
                .session
                .clone_repository(remote_url, branch.as_deref(), bearer_token.take()),
            JsonOperation::PlanMerge { .. }
            | JsonOperation::GetMergePolicy
            | JsonOperation::ValidateMergePolicy { .. }
            | JsonOperation::SetMergePolicy { .. }
            | JsonOperation::ApplyMerge { .. }
            | JsonOperation::GetMergeStatus
            | JsonOperation::ListMergePaths { .. }
            | JsonOperation::ListMergeConflicts { .. }
            | JsonOperation::ReadMergeVersion { .. }
            | JsonOperation::DiffMergeSqlite { .. }
            | JsonOperation::SetMergePathResult { .. }
            | JsonOperation::ResolveMergeRow { .. }
            | JsonOperation::ResolveMergeCell { .. }
            | JsonOperation::ResolveMergeTable { .. }
            | JsonOperation::StageMergeSqliteResult { .. }
            | JsonOperation::UnresolveMergePath { .. }
            | JsonOperation::WriteAndStageTextResult { .. }
            | JsonOperation::ContinueMerge { .. }
            | JsonOperation::AbortMerge { .. } => {
                unreachable!("handled before JSON value dispatch")
            }
        }
        .map_err(napi_error)?;
        serde_json::to_string(&value)
            .map_err(|error| Error::new(Status::GenericFailure, error.to_string()))
    }
}

enum LifecycleOperation {
    Open,
    Close,
    Reopen,
}

pub struct LifecycleTask {
    session: Arc<CoreRepositorySession>,
    operation: LifecycleOperation,
}

impl Task for LifecycleTask {
    type Output = String;
    type JsValue = String;

    fn compute(&mut self) -> Result<Self::Output> {
        match self.operation {
            LifecycleOperation::Open => self.session.open(),
            LifecycleOperation::Close => self.session.close(),
            LifecycleOperation::Reopen => self.session.reopen(),
        }
        .map_err(napi_error)?;
        Ok(lifecycle_label(self.session.lifecycle()).to_string())
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

#[napi(js_name = "RepositorySession")]
pub struct NodeRepositorySession {
    session: Arc<CoreRepositorySession>,
}

#[napi]
impl NodeRepositorySession {
    #[napi(constructor)]
    pub fn new(target: String) -> Self {
        Self {
            session: Arc::new(CoreRepositorySession::new(target)),
        }
    }

    #[napi(getter)]
    pub fn target(&self) -> String {
        self.session.target().to_string_lossy().into_owned()
    }

    #[napi(getter)]
    pub fn lifecycle(&self) -> &'static str {
        lifecycle_label(self.session.lifecycle())
    }

    #[napi]
    pub fn open(&self, signal: Option<AbortSignal>) -> AsyncTask<LifecycleTask> {
        lifecycle_task(self, LifecycleOperation::Open, signal)
    }

    #[napi]
    pub fn close(&self, signal: Option<AbortSignal>) -> AsyncTask<LifecycleTask> {
        lifecycle_task(self, LifecycleOperation::Close, signal)
    }

    #[napi]
    pub fn reopen(&self, signal: Option<AbortSignal>) -> AsyncTask<LifecycleTask> {
        lifecycle_task(self, LifecycleOperation::Reopen, signal)
    }

    #[napi]
    pub fn set_http_bearer_token(&self, remote_name: String, token: String) -> Result<()> {
        self.session
            .set_http_bearer_token(&remote_name, token)
            .map_err(napi_error)
    }

    #[napi]
    pub fn clear_http_bearer_token(&self, remote_name: String) -> Result<()> {
        self.session
            .clear_http_bearer_token(&remote_name)
            .map_err(napi_error)
    }

    #[napi]
    pub fn init(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Init, signal)
    }

    #[napi]
    pub fn status(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Status, signal)
    }

    #[napi]
    pub fn status_incremental(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::StatusIncremental, signal)
    }

    #[napi]
    pub fn repository_metadata(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::RepositoryMetadata, signal)
    }

    #[napi]
    pub fn list_remotes(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::ListRemotes, signal)
    }

    #[napi]
    pub fn add_all(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::AddAll, signal)
    }

    #[napi]
    pub fn stage_paths(
        &self,
        options: StagePathsOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::StagePaths {
                options: CoreStagePathsOptions {
                    paths: options.paths.into_iter().map(PathBuf::from).collect(),
                    expected_head: options.expected_head,
                    force: options.force.unwrap_or(false),
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn record_path_move(
        &self,
        options: RecordPathMoveOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::RecordPathMove {
                options: CoreRecordPathMoveOptions {
                    previous_path: PathBuf::from(options.previous_path),
                    path: PathBuf::from(options.path),
                    expected_head: options.expected_head,
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn untrack_paths(
        &self,
        options: UntrackPathsOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::UntrackPaths {
                options: CoreUntrackPathsOptions {
                    paths: options.paths.into_iter().map(PathBuf::from).collect(),
                    expected_head: options.expected_head,
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn commit(&self, message: String, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Commit { message }, signal)
    }

    #[napi]
    pub fn diff(
        &self,
        options: Option<DiffOptions>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        let options = options.map_or_else(CoreDiffOptions::default, |options| CoreDiffOptions {
            rows: options.rows.unwrap_or(false),
            staged: options.staged.unwrap_or(false),
            root: options.root,
            from: options.from,
            to: options.to,
            path: options.path.map(PathBuf::from),
            table: options.table,
        });
        json_task(self, JsonOperation::Diff { options }, signal)
    }

    #[napi]
    pub fn diff_paths(
        &self,
        options: DiffPathsOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::DiffPaths {
                options: CoreDiffPathsOptions {
                    paths: options.paths.into_iter().map(PathBuf::from).collect(),
                    rows: options.rows.unwrap_or(false),
                    root: options.root,
                    from: options.from,
                    to: options.to,
                    table: options.table,
                    limit: options.limit.unwrap_or(100) as usize,
                    after: options.after,
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn diff_sqlite_paths(
        &self,
        options: SqliteDiffPathsOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        let response = match options.mode.as_str() {
            "summary" => SqliteDiffResponse::Summary,
            "rows" => SqliteDiffResponse::Rows {
                table: options.table.unwrap_or_default(),
                limit: options.row_limit.unwrap_or(100) as usize,
                after: options.row_after,
            },
            _ => SqliteDiffResponse::Rows {
                table: String::new(),
                limit: 0,
                after: None,
            },
        };
        json_task(
            self,
            JsonOperation::SqliteDiffPaths {
                options: CoreSqliteDiffPathsOptions {
                    paths: options.paths.into_iter().map(PathBuf::from).collect(),
                    staged: options.staged.unwrap_or(false),
                    staged_fallback: options.staged_fallback.unwrap_or(false),
                    root: options.root,
                    from: options.from,
                    to: options.to,
                    response,
                    limit: options.limit.unwrap_or(100) as usize,
                    after: options.after,
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn read_path_content(
        &self,
        options: ReadPathContentOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::ReadPathContent {
                options: CoreReadPathContentOptions {
                    path: PathBuf::from(options.path),
                    revision: options.revision,
                    max_bytes: u64::from(options.max_bytes),
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn history(
        &self,
        limit: Option<u32>,
        after: Option<String>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::History {
                limit: limit.unwrap_or(50) as usize,
                after,
            },
            signal,
        )
    }

    #[napi]
    pub fn history_summaries(
        &self,
        limit: Option<u32>,
        after: Option<String>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::HistorySummaries {
                limit: limit.unwrap_or(50) as usize,
                after,
            },
            signal,
        )
    }

    #[napi]
    pub fn commit_details(
        &self,
        revision: String,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::CommitDetails { revision }, signal)
    }

    #[napi]
    pub fn commit_changed_paths(
        &self,
        options: CommitChangedPathsOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::CommitChangedPaths {
                options: CoreCommitChangedPathsOptions {
                    revision: options.revision,
                    limit: options.limit.unwrap_or(100) as usize,
                    after: options.after,
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn is_ignored_path(
        &self,
        path: String,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::IsIgnoredPath { path: PathBuf::from(path) },
            signal,
        )
    }

    #[napi]
    pub fn is_ignored_paths(
        &self,
        options: IgnoredPathsOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::IsIgnoredPaths {
                options: CoreIgnoredPathsOptions {
                    paths: options.paths.into_iter().map(PathBuf::from).collect(),
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn inventory(
        &self,
        options: Option<InventoryOptions>,
        signal: Option<AbortSignal>,
    ) -> Result<AsyncTask<JsonTask>> {
        let options = options.unwrap_or(InventoryOptions { kind: None, limit: None, after: None });
        let kind = match options.kind.as_deref().unwrap_or("tracked_ignored") {
            "tracked" => InventoryKind::Tracked,
            "untracked" => InventoryKind::Untracked,
            "ignored" => InventoryKind::Ignored,
            "tracked_ignored" | "trackedIgnored" => InventoryKind::TrackedIgnored,
            value => {
                return Err(Error::new(
                    Status::InvalidArg,
                    format!("unknown inventory kind `{value}`"),
                ));
            }
        };
        Ok(json_task(
            self,
            JsonOperation::Inventory {
                options: CoreInventoryOptions {
                    kind,
                    limit: options.limit.unwrap_or(100) as usize,
                    after: options.after,
                },
            },
            signal,
        ))
    }

    #[napi]
    pub fn restore(
        &self,
        options: RestoreOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::Restore {
                options: CoreRestoreOptions {
                    source: options.source,
                    expected_head: options.expected_head,
                    require_clean: options.require_clean.unwrap_or(false),
                    path: PathBuf::from(options.path),
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn restore_paths(
        &self,
        options: RestorePathsOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::RestorePaths {
                options: CoreRestorePathsOptions {
                    source: options.source,
                    expected_head: options.expected_head,
                    require_clean: options.require_clean.unwrap_or(false),
                    paths: options.paths.into_iter().map(PathBuf::from).collect(),
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn configure_remote(
        &self,
        options: RemoteConfigureOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::ConfigureRemote {
                options: CoreRemoteConfigureOptions {
                    name: options.name,
                    url: options.url,
                    bearer_token: options.bearer_token,
                    overwrite: options.overwrite.unwrap_or(false),
                    upstream_branch: options.upstream_branch,
                },
            },
            signal,
        )
    }

    #[napi]
    pub fn push(
        &self,
        remote: Option<String>,
        branch: Option<String>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Push { remote, branch }, signal)
    }

    #[napi]
    pub fn fetch(
        &self,
        remote: Option<String>,
        branch: Option<String>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Fetch { remote, branch }, signal)
    }

    #[napi]
    pub fn pull(
        &self,
        remote: Option<String>,
        branch: Option<String>,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::Pull { remote, branch }, signal)
    }

    #[napi(js_name = "planMerge")]
    pub fn plan_merge(
        &self,
        options: PlanMergeOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::PlanMerge {
                options: CorePlanMergeOptions {
                    revision: options.revision,
                    expected_head: options.expected_head,
                },
            },
            signal,
        )
    }

    #[napi(js_name = "getMergePolicy")]
    pub fn get_merge_policy(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::GetMergePolicy, signal)
    }

    #[napi(js_name = "validateMergePolicy")]
    pub fn validate_merge_policy(
        &self,
        policy_json: String,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::ValidateMergePolicy { policy_json },
            signal,
        )
    }

    #[napi(js_name = "setMergePolicy")]
    pub fn set_merge_policy(
        &self,
        options: SetMergePolicyOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::SetMergePolicy {
                policy_json: options.policy_json,
                expected_policy_token: options.expected_policy_token,
            },
            signal,
        )
    }

    #[napi(js_name = "applyMerge")]
    pub fn apply_merge(
        &self,
        options: ApplyMergeOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::ApplyMerge {
                options: CoreApplyMergeOptions {
                    revision: options.revision,
                    expected_head: options.expected_head,
                    plan_token: options.plan_token,
                },
            },
            signal,
        )
    }

    #[napi(js_name = "getMergeStatus")]
    pub fn get_merge_status(&self, signal: Option<AbortSignal>) -> AsyncTask<JsonTask> {
        json_task(self, JsonOperation::GetMergeStatus, signal)
    }

    #[napi(js_name = "listMergePaths")]
    pub fn list_merge_paths(
        &self,
        options: ListMergePathsOptions,
        signal: Option<AbortSignal>,
    ) -> Result<AsyncTask<JsonTask>> {
        let filter = match options.filter.as_str() {
            "all" => CoreMergePathFilter::All,
            "unmerged" => CoreMergePathFilter::Unmerged,
            "resolved" => CoreMergePathFilter::Resolved,
            value => return Err(invalid_enum("merge path filter", value)),
        };
        Ok(json_task(
            self,
            JsonOperation::ListMergePaths {
                options: CoreListMergePathsOptions {
                    filter,
                    limit: options.limit as usize,
                    after: options.after,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        ))
    }

    #[napi(js_name = "listMergeConflicts")]
    pub fn list_merge_conflicts(
        &self,
        options: ListMergeConflictsOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::ListMergeConflicts {
                options: CoreListMergeConflictsOptions {
                    path: PathBuf::from(options.path),
                    limit: options.limit as usize,
                    after: options.after,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        )
    }

    #[napi(js_name = "readMergeVersion")]
    pub fn read_merge_version(
        &self,
        options: ReadMergeVersionOptions,
        signal: Option<AbortSignal>,
    ) -> Result<AsyncTask<JsonTask>> {
        let version = match options.version.as_str() {
            "base" => CoreMergeVersion::Base,
            "ours" => CoreMergeVersion::Ours,
            "theirs" => CoreMergeVersion::Theirs,
            "result" => CoreMergeVersion::Result,
            value => return Err(invalid_enum("merge version", value)),
        };
        Ok(json_task(
            self,
            JsonOperation::ReadMergeVersion {
                options: CoreReadMergeVersionOptions {
                    path: PathBuf::from(options.path),
                    version,
                    max_bytes: options.max_bytes as u64,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        ))
    }

    #[napi(js_name = "diffMergeSqlite")]
    pub fn diff_merge_sqlite(
        &self,
        options: DiffMergeSqliteOptions,
        signal: Option<AbortSignal>,
    ) -> Result<AsyncTask<JsonTask>> {
        let response = match options.mode.as_str() {
            "summary" => SqliteDiffResponse::Summary,
            "rows" => SqliteDiffResponse::Rows {
                table: options.table.unwrap_or_default(),
                limit: options.row_limit.unwrap_or(100) as usize,
                after: options.row_after,
            },
            value => return Err(invalid_enum("merge SQLite diff mode", value)),
        };
        Ok(json_task(
            self,
            JsonOperation::DiffMergeSqlite {
                options: CoreDiffMergeSqliteOptions {
                    path: PathBuf::from(options.path),
                    from: parse_merge_sqlite_version(&options.from)?,
                    to: parse_merge_sqlite_version(&options.to)?,
                    response,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        ))
    }

    #[napi(js_name = "setMergePathResult")]
    pub fn set_merge_path_result(
        &self,
        options: SetMergePathResultOptions,
        signal: Option<AbortSignal>,
    ) -> Result<AsyncTask<JsonTask>> {
        let result = parse_merge_path_result(&options.result)?;
        Ok(json_task(
            self,
            JsonOperation::SetMergePathResult {
                options: CoreSetMergePathResultOptions {
                    path: PathBuf::from(options.path),
                    result,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        ))
    }

    #[napi(js_name = "resolveMergeRow")]
    pub fn resolve_merge_row(
        &self,
        options: ResolveMergeRowOptions,
        signal: Option<AbortSignal>,
    ) -> Result<AsyncTask<JsonTask>> {
        let result = parse_merge_path_result(&options.result)?;
        let identity = serde_json::from_str(&options.identity).map_err(|error| {
            Error::new(
                Status::InvalidArg,
                format!("merge row identity must be valid JSON: {error}"),
            )
        })?;
        Ok(json_task(
            self,
            JsonOperation::ResolveMergeRow {
                options: CoreResolveMergeRowOptions {
                    path: PathBuf::from(options.path),
                    table: options.table,
                    identity,
                    result,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        ))
    }

    #[napi(js_name = "resolveMergeCell")]
    pub fn resolve_merge_cell(
        &self,
        options: ResolveMergeCellOptions,
        signal: Option<AbortSignal>,
    ) -> Result<AsyncTask<JsonTask>> {
        let result = parse_merge_path_result(&options.result)?;
        let identity = serde_json::from_str(&options.identity).map_err(|error| {
            Error::new(
                Status::InvalidArg,
                format!("merge cell identity must be valid JSON: {error}"),
            )
        })?;
        Ok(json_task(
            self,
            JsonOperation::ResolveMergeCell {
                options: CoreResolveMergeCellOptions {
                    path: PathBuf::from(options.path),
                    table: options.table,
                    identity,
                    column: options.column,
                    result,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        ))
    }

    #[napi(js_name = "resolveMergeTable")]
    pub fn resolve_merge_table(
        &self,
        options: ResolveMergeTableOptions,
        signal: Option<AbortSignal>,
    ) -> Result<AsyncTask<JsonTask>> {
        let result = parse_merge_path_result(&options.result)?;
        Ok(json_task(
            self,
            JsonOperation::ResolveMergeTable {
                options: CoreResolveMergeTableOptions {
                    path: PathBuf::from(options.path),
                    table: options.table,
                    result,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        ))
    }

    #[napi(js_name = "stageMergeSqliteResult")]
    pub fn stage_merge_sqlite_result(
        &self,
        options: StageMergeSqliteResultOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::StageMergeSqliteResult {
                options: CoreStageMergeSqliteResultOptions {
                    path: PathBuf::from(options.path),
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        )
    }

    #[napi(js_name = "unresolveMergePath")]
    pub fn unresolve_merge_path(
        &self,
        options: UnresolveMergePathOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::UnresolveMergePath {
                options: CoreUnresolveMergePathOptions {
                    path: PathBuf::from(options.path),
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        )
    }

    #[napi(js_name = "writeAndStageTextResult")]
    pub fn write_and_stage_text_result(
        &self,
        options: WriteAndStageTextResultOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::WriteAndStageTextResult {
                options: CoreWriteAndStageTextResultOptions {
                    path: PathBuf::from(options.path),
                    content: options.content,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        )
    }

    #[napi(js_name = "continueMerge")]
    pub fn continue_merge(
        &self,
        options: ContinueMergeOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::ContinueMerge {
                options: CoreContinueMergeOptions {
                    message: options.message,
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        )
    }

    #[napi(js_name = "abortMerge")]
    pub fn abort_merge(
        &self,
        options: AbortMergeOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::AbortMerge {
                options: CoreAbortMergeOptions {
                    expected_state_token: options.expected_state_token,
                },
            },
            signal,
        )
    }

    #[napi(js_name = "cloneRepository")]
    pub fn clone_repository(
        &self,
        options: CloneOptions,
        signal: Option<AbortSignal>,
    ) -> AsyncTask<JsonTask> {
        json_task(
            self,
            JsonOperation::Clone {
                remote_url: options.remote_url,
                branch: options.branch,
                bearer_token: options.bearer_token,
            },
            signal,
        )
    }
}

#[napi]
pub fn operation_materializes_worktree(operation: String) -> Result<bool> {
    let operation = match operation.as_str() {
        "init" => RepositoryOperation::Init,
        "status" => RepositoryOperation::Status,
        "status_incremental" | "statusIncremental" => RepositoryOperation::StatusIncremental,
        "repository_metadata" | "repositoryMetadata" => RepositoryOperation::RepositoryMetadata,
        "list_remotes" | "listRemotes" => RepositoryOperation::ListRemotes,
        "add_all" | "addAll" => RepositoryOperation::AddAll,
        "stage_paths" | "stagePaths" => RepositoryOperation::StagePaths,
        "record_path_move" | "recordPathMove" => RepositoryOperation::RecordPathMove,
        "untrack_paths" | "untrackPaths" => RepositoryOperation::UntrackPaths,
        "commit" => RepositoryOperation::Commit,
        "diff" => RepositoryOperation::Diff,
        "diff_paths" | "diffPaths" => RepositoryOperation::DiffPaths,
        "diff_sqlite_paths" | "diffSqlitePaths" => RepositoryOperation::DiffPaths,
        "read_path_content" | "readPathContent" => RepositoryOperation::ReadPathContent,
        "history" => RepositoryOperation::History,
        "history_summaries" | "historySummaries" => RepositoryOperation::HistorySummaries,
        "commit_details" | "commitDetails" => RepositoryOperation::CommitDetails,
        "commit_changed_paths" | "commitChangedPaths" => RepositoryOperation::CommitChangedPaths,
        "is_ignored_path" | "isIgnoredPath" => RepositoryOperation::IsIgnoredPath,
        "is_ignored_paths" | "isIgnoredPaths" => RepositoryOperation::IsIgnoredPaths,
        "inventory" => RepositoryOperation::Inventory,
        "restore" => RepositoryOperation::Restore,
        "restore_paths" | "restorePaths" => RepositoryOperation::RestorePaths,
        "remote_configure" | "configureRemote" => RepositoryOperation::RemoteConfigure,
        "push" => RepositoryOperation::Push,
        "fetch" => RepositoryOperation::Fetch,
        "pull" => RepositoryOperation::Pull,
        "clone" | "cloneRepository" => RepositoryOperation::Clone,
        "plan_merge" | "planMerge" => RepositoryOperation::PlanMerge,
        "get_merge_policy" | "getMergePolicy" => RepositoryOperation::GetMergePolicy,
        "validate_merge_policy" | "validateMergePolicy" => RepositoryOperation::ValidateMergePolicy,
        "set_merge_policy" | "setMergePolicy" => RepositoryOperation::SetMergePolicy,
        "apply_merge" | "applyMerge" => RepositoryOperation::ApplyMerge,
        "get_merge_status" | "getMergeStatus" => RepositoryOperation::GetMergeStatus,
        "list_merge_paths" | "listMergePaths" => RepositoryOperation::ListMergePaths,
        "list_merge_conflicts" | "listMergeConflicts" => RepositoryOperation::ListMergeConflicts,
        "read_merge_version" | "readMergeVersion" => RepositoryOperation::ReadMergeVersion,
        "diff_merge_sqlite" | "diffMergeSqlite" => RepositoryOperation::DiffMergeSqlite,
        "set_merge_path_result" | "setMergePathResult" => RepositoryOperation::SetMergePathResult,
        "resolve_merge_row" | "resolveMergeRow" => RepositoryOperation::ResolveMergeRow,
        "resolve_merge_cell" | "resolveMergeCell" => RepositoryOperation::ResolveMergeCell,
        "resolve_merge_table" | "resolveMergeTable" => RepositoryOperation::ResolveMergeTable,
        "stage_merge_sqlite_result" | "stageMergeSqliteResult" => {
            RepositoryOperation::StageMergeSqliteResult
        }
        "unresolve_merge_path" | "unresolveMergePath" => RepositoryOperation::UnresolveMergePath,
        "write_and_stage_text_result" | "writeAndStageTextResult" => {
            RepositoryOperation::WriteAndStageTextResult
        }
        "continue_merge" | "continueMerge" => RepositoryOperation::ContinueMerge,
        "abort_merge" | "abortMerge" => RepositoryOperation::AbortMerge,
        _ => {
            return Err(Error::new(
                Status::InvalidArg,
                format!("unknown repository operation `{operation}`"),
            ));
        }
    };
    Ok(operation.materializes_worktree())
}

#[napi]
pub fn sdk_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn lifecycle_task(
    session: &NodeRepositorySession,
    operation: LifecycleOperation,
    signal: Option<AbortSignal>,
) -> AsyncTask<LifecycleTask> {
    AsyncTask::with_optional_signal(
        LifecycleTask {
            session: session.session.clone(),
            operation,
        },
        signal,
    )
}

fn json_task(
    session: &NodeRepositorySession,
    operation: JsonOperation,
    signal: Option<AbortSignal>,
) -> AsyncTask<JsonTask> {
    let cancellation = CancellationToken::new();
    if let Some(signal) = signal.as_ref() {
        let abort_token = cancellation.clone();
        signal.on_abort(move || abort_token.cancel());
    }
    AsyncTask::with_optional_signal(
        JsonTask {
            session: session.session.clone(),
            operation,
            cancellation,
        },
        signal,
    )
}

fn invalid_enum(label: &str, value: &str) -> Error {
    Error::new(Status::InvalidArg, format!("unknown {label} `{value}`"))
}

fn parse_merge_path_result(value: &str) -> Result<CoreMergePathResult> {
    match value {
        "ours" => Ok(CoreMergePathResult::Ours),
        "theirs" => Ok(CoreMergePathResult::Theirs),
        value => Err(invalid_enum("merge path result", value)),
    }
}

fn parse_merge_sqlite_version(value: &str) -> Result<CoreMergeSqliteVersion> {
    match value {
        "base" => Ok(CoreMergeSqliteVersion::Base),
        "ours" => Ok(CoreMergeSqliteVersion::Ours),
        "theirs" => Ok(CoreMergeSqliteVersion::Theirs),
        value => Err(invalid_enum("merge SQLite version", value)),
    }
}

fn napi_error(error: SdkError) -> Error {
    Error::new(
        Status::GenericFailure,
        format!("[{}] {}", error.code().as_str(), error.message()),
    )
}

fn lifecycle_label(lifecycle: graft_sdk::SessionLifecycle) -> &'static str {
    match lifecycle {
        graft_sdk::SessionLifecycle::Closed => "closed",
        graft_sdk::SessionLifecycle::Opening => "opening",
        graft_sdk::SessionLifecycle::Open => "open",
        graft_sdk::SessionLifecycle::Closing => "closing",
    }
}
