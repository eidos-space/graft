//! Repository command execution for CLI and embedding use cases.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use graft::{
    remote::{RemoteConfig, RemoteCredentialErr, RemoteCredentials},
    repo::{
        CommitFileState, CommitObject, RepoCommitChangedPathsPage, RepoHistorySummaryPage,
        RepoStatus, Repository, UserConfig,
    },
    setup::setup_graft_temporary,
};

use crate::{
    error::ErrCtx,
    page_delta::{parse_sha256_hex, sha256_hex_bytes, write_delta_from_readers},
    pragma::{
        GraftCommand,
        parse::parse_row_identity,
        repo_checkout::{export_repo_path, write_repo_file_state_to_new_path},
        repo_conflicts::{
            prepare_repo_semantic_merge_seed, resolve_repo_cell_conflict,
            stage_repo_external_sqlite_result, stage_repo_worktree_sqlite_result,
        },
        repo_core::repo_for_file,
        repo_diff::repo_status_for_file,
        spec::{
            RepoCloneSpec, RepoExportSpec, RepoResolveCellSpec, RepoResolveRowSpec,
            RepoResolveSpec, ResolveSide,
        },
        sqlite_worktree::{
            physical_sqlite_file_matches_state, prepare_cached_physical_sqlite_file_state,
        },
    },
    session::{RepoRuntimeRegistry, RepositorySessionContext},
};

/// Cloning may materialize every remote path; never merge that checkout into
/// existing user files. Hosts can use this read-only check before publication.
pub fn validate_clone_destination(worktree: &Path) -> Result<(), ErrCtx> {
    match std::fs::symlink_metadata(worktree) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(metadata) if !metadata.is_dir() => Err(ErrCtx::InvalidCommand(
            "clone destination must be an empty directory or an absent path".into(),
        )),
        Ok(_) => {
            if let Some(entry) = std::fs::read_dir(worktree)?.next() {
                entry?;
                return Err(ErrCtx::InvalidCommand(
                    "clone destination must be an empty directory or an absent path".into(),
                ));
            }
            Ok(())
        }
    }
}

/// A parsed, type-checked repository control-plane command.
///
/// Parsing is kept at the adapter boundary. Once constructed, command execution carries a typed
/// command through the service layer.
pub struct RepositoryCommand {
    command: GraftCommand,
}

/// A side from the three-way index selected by an SDK conflict resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryResolveSide {
    Ours,
    Theirs,
}

/// A stable `SQLite` row identity accepted by the repository merge driver.
#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryResolveRow {
    pub table: String,
    pub identity: serde_json::Value,
}

/// Typed conflict resolution input for embedders.
#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryResolveOptions {
    pub side: RepositoryResolveSide,
    pub path: Option<PathBuf>,
    pub row: Option<RepositoryResolveRow>,
}

/// Typed whole-table row-conflict selection for embedders.
#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryResolveTableOptions {
    pub side: RepositoryResolveSide,
    pub path: PathBuf,
    pub table: String,
}

/// Typed field-level row-conflict selection for embedders.
#[derive(Debug, Clone, PartialEq)]
pub struct RepositoryResolveCellOptions {
    pub side: RepositoryResolveSide,
    pub path: PathBuf,
    pub table: String,
    pub identity: serde_json::Value,
    pub column: String,
}

/// Immutable `SQLite` capture produced without changing refs, index, or worktree files.
#[derive(Debug, Clone)]
pub struct RepositorySqliteCapture {
    pub state: CommitFileState,
    pub content_fingerprint: String,
    pub sha256: String,
    pub bytes: u64,
    pub changed_pages: usize,
    pub page_hash_cache_hit: bool,
    pub delta: Option<RepositorySqliteDelta>,
}

/// Portable fixed-page delta between two immutable `SQLite` captures.
#[derive(Debug, Clone)]
pub struct RepositorySqliteDelta {
    pub output: PathBuf,
    pub bytes: u64,
    pub changed_pages: usize,
    pub base_content_fingerprint: String,
    pub base_sha256: String,
    pub target_sha256: String,
}

fn write_sqlite_page_delta(
    runtime: &graft::rt::runtime::Runtime,
    base: &CommitFileState,
    target: &CommitFileState,
    output: &Path,
    base_sha256: [u8; 32],
    target_sha256: [u8; 32],
) -> Result<RepositorySqliteDelta, ErrCtx> {
    let base_snapshot = base.snapshot.to_snapshot();
    let target_snapshot = target.snapshot.to_snapshot();
    let base_reader = runtime.snapshot_reader(base_snapshot.clone());
    let target_reader = runtime.snapshot_reader(target_snapshot);
    let candidates = base_reader
        .changed_page_candidates(&target_reader)?
        .into_iter()
        .collect();
    let metadata = write_delta_from_readers(
        &base_reader,
        &target_reader,
        output,
        base_sha256,
        target_sha256,
        Some(candidates),
    )?;
    Ok(RepositorySqliteDelta {
        output: output.to_path_buf(),
        bytes: metadata.delta_bytes,
        changed_pages: metadata.changed_pages as usize,
        base_content_fingerprint: format!(
            "graft-sqlite-v1:{}",
            runtime.snapshot_checksum(&base_snapshot)?
        ),
        base_sha256: metadata.base_sha256,
        target_sha256: metadata.target_sha256,
    })
}

/// The worktree effect of one structured conflict resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryResolveOutcome {
    pub path: String,
    pub materialized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySemanticMergeSeed {
    pub applied_sql: bool,
    pub managed_conflicts: usize,
}

/// The effective, versioned merge policy observed by an embedded repository session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RepositoryMergePolicy {
    pub version: u32,
    pub token: String,
    pub active_merge: bool,
    pub policy: graft::repo::MergeConfig,
}

impl RepositoryCommand {
    /// Add a remote using its explicit profile and pinned identity.
    pub fn remote_add_with_config(name: String, config: RemoteConfig) -> Self {
        Self {
            command: GraftCommand::JsonRemoteAdd { name, config },
        }
    }

    /// Clone using explicit typed remote configuration rather than a URI parser.
    pub fn clone_with_config(config: RemoteConfig, branch: Option<String>) -> Self {
        Self::clone_with_named_config("origin".into(), config, branch)
    }

    /// Preserve the trusted host's local name and credential registration during clone.
    pub fn clone_with_named_config(
        remote_name: String,
        config: RemoteConfig,
        branch: Option<String>,
    ) -> Self {
        Self {
            command: GraftCommand::JsonRepoClone {
                spec: RepoCloneSpec {
                    remote_name,
                    config,
                    branch,
                    worktree: None,
                },
            },
        }
    }

    pub fn parse(name: &str, argument: Option<&str>) -> Result<Self, ErrCtx> {
        let command = GraftCommand::parse_repository(name, argument)?;
        Ok(Self { command })
    }

    /// Creates a typed merge command for the SDK.
    pub fn merge(revision: impl Into<String>) -> Self {
        Self {
            command: GraftCommand::JsonMerge { rev: revision.into() },
        }
    }

    pub fn conflicts() -> Self {
        Self { command: GraftCommand::JsonConflicts }
    }

    pub fn resolve(options: RepositoryResolveOptions) -> Result<Self, ErrCtx> {
        let side = match options.side {
            RepositoryResolveSide::Ours => ResolveSide::Ours,
            RepositoryResolveSide::Theirs => ResolveSide::Theirs,
        };
        let row = options
            .row
            .map(|row| {
                let identity = serde_json::to_string(&row.identity)
                    .map_err(|error| ErrCtx::InvalidCommand(error.to_string().into()))?;
                Ok::<RepoResolveRowSpec, ErrCtx>(RepoResolveRowSpec {
                    table: row.table,
                    identity: parse_row_identity(&identity).map_err(|error| {
                        ErrCtx::InvalidCommand(format!("invalid row identity: {error:?}").into())
                    })?,
                })
            })
            .transpose()?;
        Ok(Self {
            command: GraftCommand::JsonResolveConflict {
                spec: RepoResolveSpec { side, path: options.path, row },
            },
        })
    }

    pub fn resolve_table(options: RepositoryResolveTableOptions) -> Self {
        let side = match options.side {
            RepositoryResolveSide::Ours => ResolveSide::Ours,
            RepositoryResolveSide::Theirs => ResolveSide::Theirs,
        };
        Self {
            command: GraftCommand::JsonResolveTableConflict {
                path: options.path,
                table: options.table,
                side,
            },
        }
    }

    pub fn unresolve(path: PathBuf) -> Self {
        Self {
            command: GraftCommand::JsonUnresolveConflict { path },
        }
    }

    pub fn record_merge_path_resolution(path: PathBuf, resolution: &'static str) -> Self {
        Self {
            command: GraftCommand::JsonRecordMergePathResolution { path, resolution },
        }
    }

    pub fn merge_continue(message: impl Into<String>) -> Self {
        Self {
            command: GraftCommand::JsonMergeContinue { message: message.into() },
        }
    }

    /// Continues after the caller has just validated an exact merge-state token and worktree.
    pub fn merge_continue_validated(message: impl Into<String>) -> Self {
        Self {
            command: GraftCommand::JsonMergeContinueValidated { message: message.into() },
        }
    }

    pub fn merge_abort() -> Self {
        Self { command: GraftCommand::JsonMergeAbort }
    }
}

/// Executes one repository command against the repository containing `target`.
///
/// The command is evaluated directly against a repository-scoped Graft runtime. `target` may be a
/// worktree database path or the repository's `.graft` directory for commands that operate on the
/// whole worktree.
pub fn execute_repository_command(
    target: &Path,
    command: RepositoryCommand,
) -> Result<Option<String>, ErrCtx> {
    let mut service = RepositoryCommandService::open(target)?;
    service.execute(command)
}

/// Executes a repository command that is proven to read only atomic refs and immutable objects.
///
/// Unlike [`execute_repository_command`], this path does not construct a repository runtime or
/// open Fjall storage. It is therefore safe to use while another process owns the repository
/// storage lock. The command itself enforces the metadata-only allowlist.
pub fn execute_repository_metadata_command(
    target: &Path,
    command: RepositoryCommand,
) -> Result<Option<String>, ErrCtx> {
    let repo = discover_target_repository(target)
        .ok_or_else(|| ErrCtx::Repo(graft::repo::RepoErr::NotFound(target.to_path_buf())))?;
    command.command.eval_metadata(&repo)
}

/// Formats a previously proven exact status without opening the repository runtime.
///
/// The caller is responsible for validating that `status` still matches repository metadata,
/// index, ignore inputs, and worktree fingerprints. This boundary retains the canonical command
/// filtering and human/JSON output shapes while avoiding a second Fjall owner.
pub fn execute_repository_persisted_status_command(
    target: &Path,
    command: RepositoryCommand,
    status: RepoStatus,
) -> Result<Option<String>, ErrCtx> {
    let repo = discover_target_repository(target)
        .ok_or_else(|| ErrCtx::Repo(graft::repo::RepoErr::NotFound(target.to_path_buf())))?;
    command.command.eval_persisted_status(&repo, status)
}

/// A long-lived repository command service.
///
/// Opening retains the repository-scoped runtime and its local storage lock until this value is
/// dropped. Callers must serialize [`Self::execute`] within one service; separate services for
/// separate repositories may execute concurrently.
pub struct RepositoryCommandService {
    file: RepositorySessionContext,
    credentials: RemoteCredentials,
}

impl RepositoryCommandService {
    /// Opens a service with the legacy CLI environment credential policy.
    pub fn open(target: &Path) -> Result<Self, ErrCtx> {
        Self::open_with_credentials(target, RemoteCredentials::environment())
    }

    /// Opens a service with an explicit credential policy supplied by an embedder.
    pub fn open_with_credentials(
        target: &Path,
        credentials: RemoteCredentials,
    ) -> Result<Self, ErrCtx> {
        Self::open_with_credentials_and_identity(target, credentials, None)
    }

    /// Opens a service with explicit credentials and an in-memory user identity override.
    pub fn open_with_credentials_and_identity(
        target: &Path,
        credentials: RemoteCredentials,
        user_identity: Option<UserConfig>,
    ) -> Result<Self, ErrCtx> {
        if let Some(user_identity) = &user_identity {
            user_identity.validate()?;
        }
        let base_runtime = setup_graft_temporary(RemoteConfig::Memory, None)?;
        let runtimes = Arc::new(RepoRuntimeRegistry::new(base_runtime.clone()));
        let repo = discover_target_repository(target).map(|repo| {
            repo.with_remote_credentials(credentials.clone())
                .with_user_identity(user_identity.clone())
        });
        #[cfg(any(unix, windows))]
        let _mutation = repo.as_ref().map(Repository::mutation_guard).transpose()?;
        let runtime = match &repo {
            Some(repo) => runtimes.runtime_for(repo)?,
            None => base_runtime,
        };
        let session_path = repo.as_ref().map_or_else(
            || target.to_path_buf(),
            |repo| repo.graft_dir().to_path_buf(),
        );
        let repository_database = repo
            .as_ref()
            .filter(|repo| target != repo.graft_dir() && !target.is_dir())
            .map(|_| target.to_path_buf());
        let mut file = RepositorySessionContext::new(
            runtime,
            session_path.to_string_lossy().into_owned(),
            repository_database,
            repo,
            runtimes,
            user_identity,
        )?;
        file.set_remote_credentials(credentials.clone());
        Ok(Self { file, credentials })
    }

    /// Executes one parsed repository command against the retained runtime.
    pub fn execute(&mut self, command: RepositoryCommand) -> Result<Option<String>, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        self.credentials.reset_http_clients();
        let runtime = self.file.runtime().clone();
        command.command.eval(&runtime, &mut self.file)
    }

    /// Native operation gate shared by typed commands and embedded SDK methods.
    /// An absent repository has no committed history to fence; initialization
    /// claims its metadata directory before its first persistent mutation.
    #[cfg(any(unix, windows))]
    pub fn mutation_guard(&self) -> Result<Option<graft::repo::RepositoryMutationGuard>, ErrCtx> {
        self.file
            .repo
            .as_ref()
            .map(Repository::mutation_guard)
            .transpose()
            .map_err(Into::into)
    }

    /// Returns the repository retained by this service, discovering it after `init` when needed.
    pub fn repository(&mut self) -> Result<Repository, ErrCtx> {
        repo_for_file(&mut self.file)
    }

    /// Publish a pinned initialization source using the retained snapshot runtime.
    #[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
    pub fn push_initialization_intent(
        &mut self,
        remote: &str,
        intent: &graft::e2ee::InitializationIntent,
    ) -> Result<graft::repo::PushOutcome, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        self.credentials.reset_http_clients();
        let repo = self.repository()?;
        if repo.branch_target(intent.branch())?.as_deref() != Some(intent.commit()) {
            return Err(graft::repo::RepoErr::from(graft::remote::RemoteErr::from(
                graft::e2ee::EncryptionError::StaleObservation,
            ))
            .into());
        }
        let observed = repo.observe_remote_branch_head(remote, intent.branch())?;
        if repo.branch_target(intent.branch())?.as_deref() != Some(intent.commit())
            || observed.head().is_some_and(|head| head != intent.commit())
        {
            return Err(graft::repo::RepoErr::from(graft::remote::RemoteErr::from(
                graft::e2ee::EncryptionError::StaleObservation,
            ))
            .into());
        }
        let runtime = self.file.runtime().clone();
        let store = Arc::new(repo.remote_store(remote)?);
        let snapshots = crate::pragma::repo_snapshot::prepare_repo_branch_snapshot_push(
            &runtime,
            &repo,
            remote,
            intent.branch(),
            observed.head(),
            store,
        )?;
        Ok(
            repo.push_initialization_intent_with_observed_head_and_snapshots(
                remote, intent, observed, snapshots,
            )?,
        )
    }

    /// Verify local migration file payloads and actual storage pages. Never hydrate
    /// from a remote or replace physical databases; cached hashes are insufficient.
    #[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
    pub fn verify_migration_source_dependencies(
        &mut self,
        plan: &graft::repo::MigrationSourcePlan,
        budget: graft::repo::MigrationVerificationBudget,
    ) -> Result<graft::repo::MigrationDependencyVerification, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        use graft::core::lsn::LSNRangeExt;
        let repo = self.repository()?;
        let payload_bytes =
            repo.verify_migration_file_dependencies(plan, budget.max_payload_bytes)?;
        let runtime = self.file.runtime().clone();
        let mut expected = std::collections::BTreeMap::new();
        for snapshot in plan.snapshots().values() {
            graft::repo::cancellation_checkpoint()?;
            if let Some(head) = snapshot.ranges.first() {
                let commit = runtime.get_commit(&head.log, head.end)?.ok_or_else(|| {
                    ErrCtx::InvalidCommand(
                        "migration snapshot references missing local head commit".into(),
                    )
                })?;
                if commit.page_count() != snapshot.page_count {
                    return Err(ErrCtx::InvalidCommand(
                        "migration snapshot page count disagrees with local storage".into(),
                    ));
                }
            } else if !snapshot.page_count.is_empty() {
                return Err(ErrCtx::InvalidCommand(
                    "migration snapshot has pages without storage ranges".into(),
                ));
            }
            for range in &snapshot.ranges {
                let mut commits = range.commits.iter();
                for lsn in (range.start..=range.end).iter() {
                    graft::repo::cancellation_checkpoint()?;
                    let commit = commits
                        .next()
                        .filter(|commit| commit.lsn == lsn)
                        .ok_or_else(|| {
                            ErrCtx::InvalidCommand(
                                "migration snapshot lacks exact storage commit identities".into(),
                            )
                        })?;
                    let key = (range.log.clone(), lsn);
                    if let Some(previous) = expected.get(&key) {
                        if previous != &commit.commit_hash {
                            return Err(ErrCtx::InvalidCommand(
                                "migration snapshots disagree on storage commit identity".into(),
                            ));
                        }
                    } else {
                        if expected.len() >= budget.max_storage_commits {
                            return Err(graft::repo::RepoErr::from(
                                graft::remote::RemoteErr::from(
                                    graft::e2ee::EncryptionError::TooLarge,
                                ),
                            )
                            .into());
                        }
                        expected.insert(key, commit.commit_hash.clone());
                    }
                }
                if commits.next().is_some() {
                    return Err(ErrCtx::InvalidCommand(
                        "migration snapshot has extra storage commit identities".into(),
                    ));
                }
            }
        }
        let mut pages = 0u64;
        for ((log, lsn), hash) in &expected {
            graft::repo::cancellation_checkpoint()?;
            let commit = runtime.get_commit(log, *lsn)?.ok_or_else(|| {
                ErrCtx::InvalidCommand(
                    "migration snapshot references missing local storage commit".into(),
                )
            })?;
            pages = pages
                .checked_add(
                    commit
                        .segment_idx()
                        .map_or(0, |index| u64::from(index.page_count().to_u32())),
                )
                .filter(|pages| *pages <= budget.max_storage_pages)
                .ok_or_else(|| {
                    graft::repo::RepoErr::from(graft::remote::RemoteErr::from(
                        graft::e2ee::EncryptionError::TooLarge,
                    ))
                })?;
            if runtime.recompute_commit_hash(log, *lsn)?.as_ref() != Some(hash) {
                return Err(ErrCtx::InvalidCommand(
                    "migration storage content does not match its authoritative identity".into(),
                ));
            }
        }
        repo.validate_migration_source_plan(plan)?;
        Ok(graft::repo::MigrationDependencyVerification {
            payloads: plan.external_payloads().len(),
            payload_bytes,
            snapshots: plan.snapshots().len(),
            storage_commits: expected.len(),
            storage_pages: pages,
        })
    }

    /// Publish all frozen historical storage commits in bounded page batches.
    /// Does not publish repository refs/objects/files or advance migration stages.
    #[cfg(all(feature = "e2ee", any(unix, windows)))]
    pub fn publish_migration_storage_dependencies(
        &mut self,
        plan: &graft::repo::MigrationSourcePlan,
        remote: Arc<graft::remote::Remote>,
        budget: graft::repo::MigrationVerificationBudget,
        max_batch_pages: u64,
    ) -> Result<graft::repo::MigrationDependencyVerification, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        remote.with_encrypted_operation(|| {
            if max_batch_pages == 0 {
                return Err(graft::repo::RepoErr::from(graft::remote::RemoteErr::from(
                    graft::e2ee::EncryptionError::TooLarge,
                ))
                .into());
            }
            let verified = self.verify_migration_source_dependencies(plan, budget)?;
            let repo = self.repository()?;
            let runtime = self.file.runtime().clone();
            let mut commits = std::collections::BTreeMap::new();
            for snapshot in plan.snapshots().values() {
                for range in &snapshot.ranges {
                    for commit in &range.commits {
                        graft::repo::cancellation_checkpoint()?;
                        commits.insert((range.log.clone(), commit.lsn), commit.commit_hash.clone());
                    }
                }
            }
            // Preflight every individual commit before any remote negotiation.
            let mut batches = Vec::new();
            let mut batch = Vec::new();
            let mut pages = 0u64;
            for ((log, lsn), hash) in commits {
                graft::repo::cancellation_checkpoint()?;
                let commit = runtime.get_commit(&log, lsn)?.ok_or_else(|| {
                    ErrCtx::InvalidCommand("migration storage commit disappeared".into())
                })?;
                let count = commit
                    .segment_idx()
                    .map_or(0, |index| u64::from(index.page_count().to_u32()));
                if count > max_batch_pages {
                    return Err(graft::repo::RepoErr::from(graft::remote::RemoteErr::from(
                        graft::e2ee::EncryptionError::TooLarge,
                    ))
                    .into());
                }
                if !batch.is_empty() && (batch.len() == 512 || count > max_batch_pages - pages) {
                    batches.push(std::mem::take(&mut batch));
                    pages = 0;
                }
                pages += count;
                batch.push((
                    graft::snapshot::Snapshot::new(log, lsn..=lsn, commit.page_count()),
                    hash,
                ));
            }
            if !batch.is_empty() {
                batches.push(batch);
            }
            repo.validate_migration_source_plan(plan)?;
            for batch in batches {
                for (snapshot, hash) in &batch {
                    graft::repo::cancellation_checkpoint()?;
                    let (log, lsn) = snapshot.head().ok_or_else(|| {
                        ErrCtx::InvalidCommand("migration storage batch is empty".into())
                    })?;
                    if runtime.recompute_commit_hash(log, lsn)?.as_ref() != Some(hash) {
                        return Err(ErrCtx::InvalidCommand(
                            "migration storage changed before publication".into(),
                        ));
                    }
                }
                runtime.snapshots_push_to(
                    batch.into_iter().map(|(snapshot, _)| snapshot).collect(),
                    remote.clone(),
                )?;
            }
            repo.validate_migration_source_plan(plan)?;
            Ok(verified)
        })
    }

    /// Publish all frozen dependencies without publishing refs or changing stages.
    #[cfg(all(feature = "e2ee", any(unix, windows)))]
    pub fn publish_migration_dependencies(
        &mut self,
        plan: &graft::repo::MigrationSourcePlan,
        remote: Arc<graft::remote::Remote>,
        budget: graft::repo::MigrationPublicationBudget,
    ) -> Result<graft::repo::MigrationDependencyPublication, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        remote.with_encrypted_operation(|| {
            if budget.max_pack_bytes == 0 || budget.max_storage_batch_pages == 0 {
                return Err(graft::repo::RepoErr::from(graft::remote::RemoteErr::from(
                    graft::e2ee::EncryptionError::TooLarge,
                ))
                .into());
            }
            let verified = self.publish_migration_storage_dependencies(
                plan,
                remote.clone(),
                budget.verification,
                budget.max_storage_batch_pages,
            )?;
            let repo = self.repository()?;
            let payload_bytes = repo.publish_migration_file_dependencies(
                plan,
                &remote,
                budget.verification.max_payload_bytes,
            )?;
            if payload_bytes != verified.payload_bytes {
                return Err(ErrCtx::InvalidCommand(
                    "migration payload inventory changed".into(),
                ));
            }
            let repository_objects =
                repo.publish_migration_repository_objects(plan, &remote, budget.max_pack_bytes)?;
            repo.validate_migration_source_plan(plan)?;
            Ok(graft::repo::MigrationDependencyPublication { repository_objects, verified })
        })
    }

    /// Fetch a branch and hydrate its target snapshots before worktree application.
    /// Plan external files and `SQLite` segments together before downloading either.
    pub fn fetch_for_checkout(
        &mut self,
        remote: &str,
        branch: &str,
    ) -> Result<graft::repo::FetchOutcome, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        let repo = self.repository()?;
        let runtime = self.file.runtime().clone();
        let prepared = repo.prepare_fetch(remote, branch)?;
        let plan = repo.plan_revision_checkout(prepared.head())?;
        let store = Arc::new(repo.remote_store(remote)?);
        let snapshots = plan
            .files
            .values()
            .map(|state| state.snapshot.to_snapshot())
            .collect::<Vec<_>>();
        let bytes = runtime.snapshots_download_bytes(&snapshots, store.clone())?;
        prepared.plan_download(bytes)?;
        for state in plan.files.values() {
            crate::pragma::repo_snapshot::hydrate_repo_file_state(
                &runtime,
                state,
                Some(store.clone()),
            )?;
        }
        Ok(prepared.finish()?)
    }

    /// Computes the repository status while retaining the service runtime.
    pub fn status(&mut self) -> Result<RepoStatus, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        let runtime = self.file.runtime().clone();
        let repo = repo_for_file(&mut self.file)?;
        repo_status_for_file(&runtime, &self.file, &repo)
    }

    /// Returns the effective policy, using the frozen snapshot while a merge is active.
    pub fn merge_policy(&mut self) -> Result<RepositoryMergePolicy, ErrCtx> {
        let repo = self.repository()?;
        if let Some((policy, token, version)) =
            crate::pragma::repo_conflicts::active_merge_policy(&repo)?
        {
            return Ok(RepositoryMergePolicy {
                version,
                token,
                active_merge: true,
                policy,
            });
        }
        let policy = repo.config()?.merge.effective();
        policy.validate()?;
        Ok(RepositoryMergePolicy {
            version: graft::repo::MERGE_POLICY_VERSION,
            token: policy.policy_token(),
            active_merge: false,
            policy,
        })
    }

    /// Applies one ours/theirs selection to a structured `SQLite` cell conflict.
    pub fn resolve_cell(
        &mut self,
        options: RepositoryResolveCellOptions,
    ) -> Result<RepositoryResolveOutcome, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        let side = match options.side {
            RepositoryResolveSide::Ours => ResolveSide::Ours,
            RepositoryResolveSide::Theirs => ResolveSide::Theirs,
        };
        let encoded = serde_json::to_string(&options.identity)
            .map_err(|error| ErrCtx::InvalidCommand(error.to_string().into()))?;
        let identity = parse_row_identity(&encoded).map_err(|error| {
            ErrCtx::InvalidCommand(format!("invalid row identity: {error:?}").into())
        })?;
        let runtime = self.file.runtime().clone();
        let repo = repo_for_file(&mut self.file)?;
        let (path, materialized) = resolve_repo_cell_conflict(
            &runtime,
            &mut self.file,
            &repo,
            &options.path,
            side,
            &RepoResolveCellSpec {
                table: options.table,
                identity,
                column: options.column,
            },
        )?;
        Ok(RepositoryResolveOutcome { path, materialized })
    }

    /// Validates and stages the current physical `SQLite` file as the resolved merge result.
    pub fn stage_worktree_sqlite_result(&mut self, path: &Path) -> Result<String, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        let runtime = self.file.runtime().clone();
        let repo = repo_for_file(&mut self.file)?;
        stage_repo_worktree_sqlite_result(&runtime, &repo, path)
    }

    /// Exports one immutable revision path to a standalone physical `SQLite` file.
    pub fn export_revision_sqlite_path(
        &mut self,
        revision: &str,
        path: &Path,
        output: &Path,
    ) -> Result<String, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        let runtime = self.file.runtime().clone();
        let repo = repo_for_file(&mut self.file)?;
        export_repo_path(
            &runtime,
            &self.file,
            &repo,
            &RepoExportSpec {
                source: Some(revision.to_string()),
                path: Some(repo.worktree().join(path)),
                output: output.to_path_buf(),
            },
        )
    }

    /// Captures one physical `SQLite` worktree file into `Graft` storage and exports that exact
    /// immutable image. Supplying a prior capture allows unchanged pages to remain shared.
    pub fn capture_worktree_sqlite(
        &mut self,
        path: &Path,
        output: &Path,
        base: Option<&CommitFileState>,
        base_sha256: Option<&str>,
        delta_output: Option<&Path>,
    ) -> Result<RepositorySqliteCapture, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        let runtime = self.file.runtime().clone();
        let repo = repo_for_file(&mut self.file)?;
        let physical_path = repo.worktree().join(path);
        let key = repo.file_key(&physical_path)?;
        let (state, prepared) = prepare_cached_physical_sqlite_file_state(
            &runtime,
            &repo,
            &format!("sdk-capture:{key}"),
            &physical_path,
            base,
        )?;
        let target_sha256 = write_repo_file_state_to_new_path(&runtime, &state, output)?;
        let sha256 = sha256_hex_bytes(&target_sha256);
        let content_fingerprint = format!(
            "graft-sqlite-v1:{}",
            runtime.snapshot_checksum(&state.snapshot.to_snapshot())?
        );
        let bytes = std::fs::metadata(output)?.len();
        let delta = match (base, delta_output) {
            (Some(base), Some(delta_output)) => {
                let Some(base_sha256) = base_sha256 else {
                    let _ = std::fs::remove_file(output);
                    return Err(ErrCtx::InvalidCommand(
                        "SQLite delta capture requires the base snapshot SHA-256".into(),
                    ));
                };
                let base_sha256 = match parse_sha256_hex(base_sha256) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = std::fs::remove_file(output);
                        return Err(error);
                    }
                };
                match write_sqlite_page_delta(
                    &runtime,
                    base,
                    &state,
                    delta_output,
                    base_sha256,
                    target_sha256,
                ) {
                    Ok(delta) => Some(delta),
                    Err(error) => {
                        let _ = std::fs::remove_file(output);
                        return Err(error);
                    }
                }
            }
            _ => None,
        };
        Ok(RepositorySqliteCapture {
            state,
            content_fingerprint,
            sha256,
            bytes,
            changed_pages: prepared.changed_page_count(),
            page_hash_cache_hit: prepared.page_hash_cache_hit(),
            delta,
        })
    }

    /// Validates a provider-owned candidate, materializes it, and stages it as one merge result.
    pub fn stage_external_sqlite_result(
        &mut self,
        path: &Path,
        candidate_path: &Path,
    ) -> Result<String, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        let runtime = self.file.runtime().clone();
        let repo = repo_for_file(&mut self.file)?;
        stage_repo_external_sqlite_result(&runtime, &repo, path, candidate_path)
    }

    /// Builds an Ours-derived private candidate containing every safe non-provider row change.
    pub fn prepare_semantic_merge_seed(
        &mut self,
        path: &Path,
        candidate_path: &Path,
        managed_tables: &std::collections::BTreeSet<String>,
    ) -> Result<RepositorySemanticMergeSeed, ErrCtx> {
        #[cfg(any(unix, windows))]
        let _mutation = self.mutation_guard()?;
        let runtime = self.file.runtime().clone();
        let repo = repo_for_file(&mut self.file)?;
        let (applied_sql, managed_conflicts) = prepare_repo_semantic_merge_seed(
            &runtime,
            &self.file,
            &repo,
            path,
            candidate_path,
            managed_tables,
        )?;
        Ok(RepositorySemanticMergeSeed { applied_sql, managed_conflicts })
    }

    /// Lists commit metadata without hydrating any commit trees or blobs.
    pub fn history_summaries(
        &mut self,
        limit: usize,
        after: Option<&str>,
    ) -> Result<RepoHistorySummaryPage, ErrCtx> {
        self.repository()?
            .history_summary_page(limit, after)
            .map_err(Into::into)
    }

    /// Hydrates the full details for one commit on demand.
    pub fn commit_details(&mut self, revision: &str) -> Result<CommitObject, ErrCtx> {
        let repo = self.repository()?;
        let id = repo.resolve_revision(revision)?;
        repo.read_commit(&id).map_err(Into::into)
    }

    /// Lazily hydrates one commit's first-parent path changes and returns a bounded page.
    pub fn commit_changed_paths(
        &mut self,
        revision: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<RepoCommitChangedPathsPage, ErrCtx> {
        self.repository()?
            .commit_changed_paths_page(revision, limit, after)
            .map_err(Into::into)
    }

    /// Compares a physical `SQLite` worktree file with its tracked snapshot.
    pub fn physical_sqlite_matches(
        &self,
        path: &Path,
        expected: &CommitFileState,
    ) -> Result<bool, ErrCtx> {
        physical_sqlite_file_matches_state(self.file.runtime(), path, expected)
    }

    /// Injects or rotates an HTTP bearer token without writing it to repository config.
    pub fn set_http_bearer_token(
        &self,
        remote_name: &str,
        token: String,
    ) -> Result<(), RemoteCredentialErr> {
        self.credentials.set_http_bearer_token(remote_name, token)
    }

    /// Clears an injected HTTP bearer token.
    pub fn clear_http_bearer_token(&self, remote_name: &str) -> Result<(), RemoteCredentialErr> {
        self.credentials.clear_http_bearer_token(remote_name)
    }

    /// Redacts all credentials held by this service from a diagnostic string.
    pub fn redact(&self, message: &str) -> String {
        self.credentials.redact(message)
    }
}

fn discover_target_repository(target: &Path) -> Option<Repository> {
    if target
        .file_name()
        .is_some_and(|name| name == graft::repo::GRAFT_DIR)
    {
        return target
            .parent()
            .and_then(|parent| Repository::discover(parent).ok());
    }
    Repository::discover_for_file(target).ok()
}
