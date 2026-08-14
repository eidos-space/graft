use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use graft::{
    GraftErr,
    core::VolumeId,
    remote::RemoteCredentials,
    repo::{CommitFileState, CommitTableSummary, Repository},
    rt::runtime::Runtime,
    snapshot::Snapshot,
    volume_reader::VolumeReadRef,
};
use parking_lot::Mutex;

use crate::{error::ErrCtx, pragma::sqlite_worktree::PreparedSqliteStage, row_merge::RowMergePlan};

#[derive(Debug)]
pub(crate) struct RepoRuntimeRegistry {
    base: Runtime,
    runtimes: Mutex<HashMap<PathBuf, Runtime>>,
}

impl RepoRuntimeRegistry {
    pub(crate) fn new(base: Runtime) -> Self {
        Self { base, runtimes: Default::default() }
    }

    pub(crate) fn runtime_for(&self, repo: &Repository) -> Result<Runtime, ErrCtx> {
        let key = repo.graft_dir().to_path_buf();
        if let Some(runtime) = self.runtimes.lock().get(&key) {
            return Ok(runtime.clone());
        }

        let runtime = self
            .base
            .fork_with_storage_path(repo.store_dir())
            .map_err(GraftErr::from)?;
        self.runtimes.lock().insert(key, runtime.clone());
        Ok(runtime)
    }
}

/// Repository-scoped state shared by CLI and SDK commands.
///
/// It owns the runtime, selected snapshot volume, repository metadata, and staged `SQLite`
/// snapshot cache needed by repository commands.
pub(crate) struct RepositorySessionContext {
    runtime: Runtime,
    pub(crate) tag: String,
    pub(crate) vid: VolumeId,
    pub(crate) repo: Option<Repository>,
    remote_credentials: RemoteCredentials,
    repo_runtimes: Arc<RepoRuntimeRegistry>,
    repository_database: Option<PathBuf>,
    prepared_sqlite_stages: Mutex<BTreeMap<String, Arc<PreparedSqliteStage>>>,
    row_merge_plans: Mutex<BTreeMap<String, Arc<RowMergePlan>>>,
    row_merge_table_summaries: Mutex<BTreeMap<String, CachedRowMergeTableSummaries>>,
}

#[derive(Clone)]
struct CachedRowMergeTableSummaries {
    from: CommitFileState,
    to: CommitFileState,
    tables: Vec<CommitTableSummary>,
}

impl RepositorySessionContext {
    pub(crate) fn new(
        runtime: Runtime,
        tag: String,
        repository_database: Option<PathBuf>,
        repo: Option<Repository>,
        repo_runtimes: Arc<RepoRuntimeRegistry>,
    ) -> Result<Self, ErrCtx> {
        let volume = runtime.volume_open(None, None, None)?;
        Ok(Self {
            runtime,
            tag,
            vid: volume.vid,
            repo,
            remote_credentials: RemoteCredentials::environment(),
            repo_runtimes,
            repository_database,
            prepared_sqlite_stages: Mutex::new(BTreeMap::new()),
            row_merge_plans: Mutex::new(BTreeMap::new()),
            row_merge_table_summaries: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub(crate) fn cache_prepared_sqlite_stage(&self, key: String, prepared: PreparedSqliteStage) {
        self.prepared_sqlite_stages
            .lock()
            .insert(key, Arc::new(prepared));
    }

    pub(crate) fn prepared_sqlite_stage(&self, key: &str) -> Option<Arc<PreparedSqliteStage>> {
        self.prepared_sqlite_stages.lock().get(key).cloned()
    }

    pub(crate) fn clear_prepared_sqlite_stages(&self) {
        self.prepared_sqlite_stages.lock().clear();
    }

    pub(crate) fn row_merge_plan(&self, key: &str) -> Option<Arc<RowMergePlan>> {
        self.row_merge_plans.lock().get(key).cloned()
    }

    pub(crate) fn cache_row_merge_plan(&self, key: String, plan: Arc<RowMergePlan>) {
        let mut plans = self.row_merge_plans.lock();
        if plans.len() >= 32 {
            plans.clear();
        }
        plans.insert(key, plan);
    }

    pub(crate) fn cache_row_merge_table_summaries(
        &self,
        key: String,
        from: CommitFileState,
        to: CommitFileState,
        tables: Vec<CommitTableSummary>,
    ) {
        self.row_merge_table_summaries
            .lock()
            .insert(key, CachedRowMergeTableSummaries { from, to, tables });
    }

    pub(crate) fn row_merge_table_summaries(
        &self,
        key: &str,
        from: &CommitFileState,
        to: &CommitFileState,
    ) -> Option<Vec<CommitTableSummary>> {
        self.row_merge_table_summaries
            .lock()
            .get(key)
            .filter(|cached| &cached.from == from && &cached.to == to)
            .map(|cached| cached.tables.clone())
    }

    pub(crate) fn clear_row_merge_table_summaries(&self) {
        self.row_merge_table_summaries.lock().clear();
    }

    pub(crate) fn repository_database_path(&self) -> Option<&Path> {
        self.repository_database.as_deref()
    }

    pub(crate) fn remote_credentials(&self) -> &RemoteCredentials {
        &self.remote_credentials
    }

    pub(crate) fn set_remote_credentials(&mut self, credentials: RemoteCredentials) {
        self.remote_credentials = credentials.clone();
        self.repo = self
            .repo
            .take()
            .map(|repo| repo.with_remote_credentials(credentials));
    }

    pub(crate) fn attach_repo(&mut self, repo: Repository) -> Result<(), ErrCtx> {
        let repo = repo.with_remote_credentials(self.remote_credentials.clone());
        let runtime = self.repo_runtimes.runtime_for(&repo)?;
        self.switch_runtime(runtime)?;
        self.repo = Some(repo);
        Ok(())
    }

    pub(crate) fn attach_repo_preserving_contents(
        &mut self,
        repo: Repository,
    ) -> Result<bool, ErrCtx> {
        self.attach_repo(repo)?;
        Ok(false)
    }

    fn switch_runtime(&mut self, runtime: Runtime) -> Result<(), ErrCtx> {
        self.vid = runtime.volume_open(None, None, None)?.vid;
        self.runtime = runtime;
        Ok(())
    }

    pub(crate) fn snapshot_or_latest(&self) -> Result<Snapshot, ErrCtx> {
        Ok(self.runtime.volume_snapshot(&self.vid)?)
    }

    pub(crate) const fn is_idle(&self) -> bool {
        true
    }

    pub(crate) fn switch_volume(&mut self, vid: &VolumeId) -> Result<(), ErrCtx> {
        self.vid = vid.clone();
        Ok(())
    }

    pub(crate) fn clear_volume_binding(&mut self) -> Result<(), ErrCtx> {
        self.vid = self.runtime.volume_open(None, None, None)?.vid;
        Ok(())
    }

    pub(crate) fn reader(&self) -> Result<VolumeReadRef<'_>, ErrCtx> {
        Ok(VolumeReadRef::Reader(std::borrow::Cow::Owned(
            self.runtime.volume_reader(self.vid.clone())?,
        )))
    }
}

pub(crate) fn should_discover_repo(tag: &str) -> bool {
    let path = Path::new(tag);
    path.is_absolute() || tag.contains('/') || tag.contains('\\') || path.extension().is_some()
}
