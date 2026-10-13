//! Exclusive, exact-intent orchestration of already prepared migration journals.
//! Publication completion is deliberately distinct from independent recovery and cutover.
use crate::{
    RemoteConfig, RemoteCredentials, RepositorySession, Result, SdkError, SdkErrorCode, e2ee,
};
use e2ee::{
    EncryptionError, FileInitializationJournal, FileMigrationEnrollmentJournal,
    FileMigrationProgressJournal, InitialConfigPublicationOutcome, InitializationJournal,
    InitializationRecord, InitializationStage, MigrationProgressLease, SecretSession,
    SessionBinding,
};
use graft::{
    remote::Remote,
    repo::{
        MigrationPublicationBudget, MigrationSourcePlan, MigrationTargetPlan,
        MigrationVerificationBudget,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationJournalPaths {
    pub source: PathBuf,
    pub target: PathBuf,
    pub enrollment: PathBuf,
    pub initialization: PathBuf,
    pub progress: PathBuf,
}

/// All journals must already be durably prepared offline. This operation never
/// generates a root, changes source selection or manufactures missing journals.
pub struct MigrationCoordinatorOptions {
    pub remote_name: String,
    pub config: RemoteConfig,
    pub journals: MigrationJournalPaths,
    pub budget: MigrationPublicationBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationPublicationState {
    Prepared,
    ConfigConfirmed,
    DependenciesConfirmed,
    PublishingRefs {
        confirmed: u32,
        total: u32,
    },
    /// All selected refs and HEAD were confirmed. Independent recovery and an
    /// explicit host write fence/device cutover remain required.
    ReadyForIndependentVerification,
    /// The trusted host durably switched its configured target while fenced.
    /// This does not claim that offline devices stopped writing or were revoked.
    HostCutoverConfirmed,
}

/// An actual host writer-fence capability, held continuously from independent
/// recovery through durable configuration cutover. Implementations must exclude
/// every supported source/ref writer, including other processes. A prompt,
/// boolean, standalone advisory marker or a JavaScript assertion is not a fence.
pub trait MigrationWriteFence {
    fn check_held(&self) -> e2ee::Result<()>;

    /// Bind the capability to this canonical source; a guard for an unrelated
    /// repository must never authorize its cutover.
    fn covers_repository(&self, repository: &graft::repo::Repository) -> bool;

    /// Atomically persist the host target switch. Retrying the same intent ID
    /// must succeed idempotently, including after process death before the
    /// coordinator records its success. Never rewrite the retained source refs.
    fn commit_cutover(&mut self, intent: MigrationCutoverIntent<'_>) -> e2ee::Result<()>;
}

/// Private host configuration input. This is deliberately not serializable.
pub struct MigrationCutoverIntent<'a> {
    pub id: [u8; 32],
    pub remote_name: &'a str,
    pub config: &'a RemoteConfig,
    pub target: &'a MigrationTargetPlan,
}

/// Canonical local repository fence for supported Graft metadata commands.
/// This capability is thread-bound and unavailable to Node/plugin RPC. It does
/// not lock arbitrary external file/`SQLite` writers or other devices; cutover
/// preserves the physical worktree and switches only remote/upstream config.
pub struct RepositoryMigrationFence {
    repository: graft::repo::Repository,
    guard: graft::repo::RepositoryMutationGuard,
    intent: Option<[u8; 32]>,
}

impl RepositoryMigrationFence {
    pub fn acquire(source: &RepositorySession) -> Result<Self> {
        source.with_service(|service| {
            let repository = service
                .repository()
                .map_err(crate::repository_command_error)?;
            let guard = repository.mutation_guard().map_err(crate::repo_error)?;
            Ok(Self { repository, guard, intent: None })
        })
    }
}

impl MigrationWriteFence for RepositoryMigrationFence {
    fn check_held(&self) -> e2ee::Result<()> {
        self.guard.check_held().map_err(fence_error)
    }

    fn covers_repository(&self, repository: &graft::repo::Repository) -> bool {
        self.repository.graft_dir() == repository.graft_dir()
    }

    fn commit_cutover(&mut self, intent: MigrationCutoverIntent<'_>) -> e2ee::Result<()> {
        self.check_held()?;
        if self.intent.is_some_and(|previous| previous != intent.id) {
            return Err(EncryptionError::StaleObservation);
        }
        self.intent = Some(intent.id);
        let mut config = self.repository.config().map_err(fence_error)?;
        config
            .remotes
            .insert(intent.remote_name.to_owned(), intent.config.clone());
        for target in intent.target.references().values() {
            if let Some(branch) = target.source_ref.strip_prefix("refs/heads/") {
                config.branches.insert(
                    branch.to_owned(),
                    graft::repo::BranchConfig {
                        remote: Some(intent.remote_name.to_owned()),
                        merge: Some(target.target_ref.clone()),
                    },
                );
            }
        }
        // One atomic, fsynced configuration replacement includes every mapped
        // local upstream. Replaying the same exact mapping is idempotent.
        self.repository.write_config(&config).map_err(fence_error)?;
        self.check_held()
    }
}

fn fence_error(error: graft::repo::RepoErr) -> EncryptionError {
    match error {
        graft::repo::RepoErr::MutationBusy => EncryptionError::SessionBusy,
        graft::repo::RepoErr::Io(error) => EncryptionError::CheckpointUnavailable(error.kind()),
        _ => EncryptionError::StaleObservation,
    }
}

/// Fresh private directories for a recovery attempt. A failed attempt consumes
/// these directories; retry with new empty directories, never source caches.
pub struct MigrationVerificationOptions {
    pub worktree: PathBuf,
    pub checkpoint: PathBuf,
    pub transfer: crate::EncryptedTransferBudget,
    pub bearer_token: String,
}

/// Evidence retained only in this process, bound to one exact migration intent.
/// It is not serializable and does not establish a host write fence.
pub struct IndependentMigrationVerification {
    intent: [u8; 32],
    pub references: usize,
    pub repository_objects: usize,
    pub dependencies: graft::repo::MigrationDependencyVerification,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedIntent {
    version: u32,
    remote_name: String,
    config: RemoteConfig,
    journals: MigrationJournalPaths,
    source_digest: [u8; 32],
    target_digest: [u8; 32],
    enrollment_digest: [u8; 32],
    config_digest: [u8; 32],
    max_payload_bytes: u64,
    max_storage_commits: usize,
    max_storage_pages: u64,
    max_storage_batch_pages: u64,
    max_pack_bytes: usize,
}

impl RetainedIntent {
    fn budget(&self) -> MigrationPublicationBudget {
        MigrationPublicationBudget {
            verification: MigrationVerificationBudget {
                max_payload_bytes: self.max_payload_bytes,
                max_storage_commits: self.max_storage_commits,
                max_storage_pages: self.max_storage_pages,
            },
            max_storage_batch_pages: self.max_storage_batch_pages,
            max_pack_bytes: self.max_pack_bytes,
        }
    }
}

/// Opaque trusted-host coordinator. Its lease serializes processes sharing this
/// progress journal; it does not claim to pause application or offline writers.
pub struct EncryptedMigrationCoordinator {
    source: Arc<RepositorySession>,
    secret: SecretSession,
    lease: Arc<MigrationProgressLease>,
    intent: RetainedIntent,
    source_plan: MigrationSourcePlan,
    target_plan: MigrationTargetPlan,
    remote: Arc<Remote>,
    client: e2ee::EncryptedHttpClient,
    initialization: Arc<LeasedInitialization>,
}

struct LeasedInitialization {
    inner: Arc<e2ee::MigrationInitializationJournal>,
    // Retain exclusion inside admitted native publication tasks, even when the
    // caller drops its future and coordinator while native work is draining.
    lease: Arc<MigrationProgressLease>,
}

impl InitializationJournal for LeasedInitialization {
    fn load(&self, binding: &SessionBinding) -> e2ee::Result<InitializationRecord> {
        self.lease.snapshot()?;
        self.inner.load(binding)
    }
    fn advance(
        &self,
        binding: &SessionBinding,
        expected: InitializationStage,
        next: InitializationStage,
    ) -> e2ee::Result<()> {
        self.lease.snapshot()?;
        self.inner.advance(binding, expected, next)
    }
}

fn encrypted(error: EncryptionError) -> SdkError {
    SdkError::new(
        crate::encryption_error_code(&error),
        "migration private state unavailable",
    )
}
fn invalid() -> SdkError {
    encrypted(EncryptionError::StaleObservation)
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

fn validate_paths(source: &RepositorySession, paths: &MigrationJournalPaths) -> Result<()> {
    let directories = [
        &paths.source,
        &paths.target,
        &paths.enrollment,
        &paths.initialization,
        &paths.progress,
    ];
    let canonical: Vec<PathBuf> = directories
        .iter()
        .map(|path| {
            if !path.is_absolute() {
                return Err(invalid());
            }
            std::fs::canonicalize(path)
                .map_err(|error| encrypted(EncryptionError::CheckpointUnavailable(error.kind())))
        })
        .collect::<Result<_>>()?;
    source
        .validate_encrypted_host_directories(&canonical)
        .map_err(encrypted)?;
    for (index, path) in canonical.iter().enumerate() {
        if canonical[..index]
            .iter()
            .any(|other| path.starts_with(other) || other.starts_with(path))
        {
            return Err(encrypted(EncryptionError::InvalidSessionBinding));
        }
    }
    Ok(())
}

impl EncryptedMigrationCoordinator {
    pub fn prepare(
        source: Arc<RepositorySession>,
        secret: SecretSession,
        credentials: RemoteCredentials,
        options: MigrationCoordinatorOptions,
    ) -> Result<Self> {
        validate_paths(&source, &options.journals)?;
        let initial =
            FileInitializationJournal::open(&options.journals.initialization, secret.binding())
                .map_err(encrypted)?;
        let initial_record = initial.load(secret.binding()).map_err(encrypted)?;
        if initial_record.stage != InitializationStage::Prepared || initial_record.intent.is_some()
        {
            return Err(invalid());
        }
        let source_plan =
            source.restore_migration_source_journal(&options.journals.source, secret.binding())?;
        let target_plan = source.restore_migration_enrollment_journal(
            &options.journals.enrollment,
            &options.journals.target,
            &secret,
            &initial,
        )?;
        let enrollment =
            FileMigrationEnrollmentJournal::open(&options.journals.enrollment, secret.binding())
                .map_err(encrypted)?
                .load(secret.binding())
                .map_err(encrypted)?;
        let intent = RetainedIntent {
            version: 1,
            remote_name: options.remote_name,
            config: options.config,
            journals: options.journals,
            source_digest: digest(&source_plan.encode_private().map_err(crate::repo_error)?),
            target_digest: digest(&target_plan.encode_private().map_err(crate::repo_error)?),
            enrollment_digest: digest(&enrollment),
            config_digest: digest(initial_record.config.as_bytes()),
            max_payload_bytes: options.budget.verification.max_payload_bytes,
            max_storage_commits: options.budget.verification.max_storage_commits,
            max_storage_pages: options.budget.verification.max_storage_pages,
            max_storage_batch_pages: options.budget.max_storage_batch_pages,
            max_pack_bytes: options.budget.max_pack_bytes,
        };
        validate_intent(&intent, &secret)?;
        // Validate bound transport and durable ref support before allocating progress.
        let remote = intent
            .config
            .clone()
            .build_with_credentials(&intent.remote_name, &credentials)
            .map_err(|error| crate::repo_error(error.into()))?;
        remote
            .require_encrypted_ref_journals()
            .map_err(|error| crate::repo_error(error.into()))?;
        validate_transport(&source, &intent, &remote, &initial_record.config)?;
        let bytes = serde_json::to_vec(&intent).map_err(|_| invalid())?;
        let total = u32::try_from(target_plan.references().len())
            .ok()
            .and_then(|count| count.checked_add(4))
            .ok_or_else(invalid)?;
        let lease = Arc::new(
            FileMigrationProgressJournal::create(
                &intent.journals.progress,
                secret.binding(),
                &bytes,
                total,
            )
            .map_err(encrypted)?
            .acquire(secret.binding())
            .map_err(encrypted)?,
        );
        Self::restore(source, secret, credentials, lease)
    }

    /// Resume uses only the retained endpoint, mapping, journals and budgets.
    /// Current HEAD, caller defaults and different credentials cannot replan it.
    pub fn resume(
        source: Arc<RepositorySession>,
        secret: SecretSession,
        credentials: RemoteCredentials,
        progress_directory: &Path,
    ) -> Result<Self> {
        source
            .validate_encrypted_host_directories(&[progress_directory.to_owned()])
            .map_err(encrypted)?;
        let lease = Arc::new(
            FileMigrationProgressJournal::open(progress_directory, secret.binding())
                .map_err(encrypted)?
                .acquire(secret.binding())
                .map_err(encrypted)?,
        );
        Self::restore(source, secret, credentials, lease)
    }

    fn restore(
        source: Arc<RepositorySession>,
        secret: SecretSession,
        credentials: RemoteCredentials,
        lease: Arc<MigrationProgressLease>,
    ) -> Result<Self> {
        let record = lease.snapshot().map_err(encrypted)?;
        let intent: RetainedIntent =
            serde_json::from_slice(record.intent()).map_err(|_| invalid())?;
        if serde_json::to_vec(&intent).map_err(|_| invalid())? != record.intent() {
            return Err(invalid());
        }
        validate_intent(&intent, &secret)?;
        validate_paths(&source, &intent.journals)?;
        let initial = Arc::new(
            FileInitializationJournal::open(&intent.journals.initialization, secret.binding())
                .map_err(encrypted)?,
        );
        let initial_record = initial.load(secret.binding()).map_err(encrypted)?;
        let source_plan =
            source.restore_migration_source_journal(&intent.journals.source, secret.binding())?;
        let target_plan = source.restore_migration_enrollment_journal(
            &intent.journals.enrollment,
            &intent.journals.target,
            &secret,
            initial.as_ref(),
        )?;
        let enrollment =
            FileMigrationEnrollmentJournal::open(&intent.journals.enrollment, secret.binding())
                .map_err(encrypted)?
                .load(secret.binding())
                .map_err(encrypted)?;
        if digest(&source_plan.encode_private().map_err(crate::repo_error)?) != intent.source_digest
            || digest(&target_plan.encode_private().map_err(crate::repo_error)?)
                != intent.target_digest
            || digest(&enrollment) != intent.enrollment_digest
            || digest(initial_record.config.as_bytes()) != intent.config_digest
            || u32::try_from(target_plan.references().len())
                .ok()
                .and_then(|count| count.checked_add(4))
                != Some(record.total_steps())
        {
            return Err(invalid());
        }
        let remote = Arc::new(
            intent
                .config
                .clone()
                .build_with_credentials(&intent.remote_name, &credentials)
                .map_err(|error| crate::repo_error(error.into()))?,
        );
        remote
            .require_encrypted_ref_journals()
            .map_err(|error| crate::repo_error(error.into()))?;
        validate_transport(&source, &intent, &remote, &initial_record.config)?;
        let RemoteConfig::EncryptedHttp { url, .. } = &intent.config else {
            return Err(invalid());
        };
        let client = e2ee::EncryptedHttpClient::new(
            url,
            intent.remote_name.clone(),
            credentials,
            secret.clone(),
        )
        .map_err(http_error)?;
        let inner = source.migration_initialization_journal(
            &intent.journals.enrollment,
            &intent.journals.target,
            secret.clone(),
            initial,
        )?;
        let initialization = Arc::new(LeasedInitialization { inner, lease: lease.clone() });
        Ok(Self {
            source,
            secret,
            lease,
            intent,
            source_plan,
            target_plan,
            remote,
            client,
            initialization,
        })
    }

    pub fn state(&self) -> Result<MigrationPublicationState> {
        let record = self.lease.snapshot().map_err(encrypted)?;
        let refs = record.total_steps() - 4;
        Ok(match record.completed_steps() {
            0 => MigrationPublicationState::Prepared,
            1 => MigrationPublicationState::ConfigConfirmed,
            2 => MigrationPublicationState::DependenciesConfirmed,
            completed if completed == record.total_steps() => {
                MigrationPublicationState::HostCutoverConfirmed
            }
            completed if completed == record.total_steps() - 1 => {
                MigrationPublicationState::ReadyForIndependentVerification
            }
            completed => {
                MigrationPublicationState::PublishingRefs { confirmed: completed - 2, total: refs }
            }
        })
    }

    /// Execute one fixed phase. Publication states are hints for safe replay,
    /// never a claim of independent restoration, a write fence or device cutover.
    pub fn publish_next(&mut self) -> Result<MigrationPublicationState> {
        let operation = self.secret.begin_operation().map_err(encrypted)?;
        self.source
            .validate_migration_source_plan(&self.source_plan)?;
        self.source
            .validate_migration_target_plan(&self.target_plan)?;
        let record = self.lease.snapshot().map_err(encrypted)?;
        let completed = record.completed_steps();
        if completed >= record.total_steps() - 1 {
            return self.state();
        }
        if completed == 0 {
            let outcome = run_http(
                self.client
                    .publish_initial_config(self.initialization.clone()),
            )?;
            match outcome {
                InitialConfigPublicationOutcome::Confirmed => {}
                InitialConfigPublicationOutcome::Unconfirmed => {
                    return Err(SdkError::new(
                        SdkErrorCode::RemotePublicationUnconfirmed,
                        "migration configuration publication unconfirmed",
                    ));
                }
                InitialConfigPublicationOutcome::Conflict => return Err(invalid()),
                InitialConfigPublicationOutcome::OutcomeUnknown { .. } => {
                    return Err(SdkError::new(
                        SdkErrorCode::RemotePublicationOutcomeUnknown,
                        "migration configuration publication outcome unknown",
                    ));
                }
            }
        } else {
            run_http(self.client.verify())?;
            if completed == 1 {
                self.source.publish_migration_dependencies(
                    &self.source_plan,
                    self.remote.clone(),
                    self.intent.budget(),
                )?;
            } else if completed < record.total_steps() - 2 {
                let target = self
                    .target_plan
                    .references()
                    .values()
                    .nth((completed - 2) as usize)
                    .ok_or_else(invalid)?;
                if target.target_ref.starts_with("refs/heads/") {
                    self.source.publish_migration_target_branch(
                        &self.target_plan,
                        self.remote.clone(),
                        &target.target_ref,
                    )?;
                } else {
                    self.source.publish_migration_target_tag(
                        &self.target_plan,
                        self.remote.clone(),
                        &target.target_ref,
                    )?;
                }
            } else {
                self.source
                    .publish_migration_target_head(&self.target_plan, self.remote.clone())?;
            }
        }
        operation.check_active().map_err(encrypted)?;
        self.source
            .validate_migration_source_plan(&self.source_plan)?;
        self.lease.confirm_step(completed).map_err(encrypted)?;
        self.state()
    }

    /// Recover all selected history using a fresh password unlock, checkpoint,
    /// object repository and `SQLite` runtime. No source cache supplies content.
    pub fn verify_independently(
        &mut self,
        password: zeroize::Zeroizing<Vec<u8>>,
        options: MigrationVerificationOptions,
    ) -> Result<IndependentMigrationVerification> {
        if !matches!(
            self.state()?,
            MigrationPublicationState::ReadyForIndependentVerification
                | MigrationPublicationState::HostCutoverConfirmed
        ) {
            return Err(invalid());
        }
        let operation = self.secret.begin_operation().map_err(encrypted)?;
        self.source
            .validate_migration_source_plan(&self.source_plan)?;
        // Confirm the source closure fits every retained limit before networking.
        self.source.verify_migration_source_dependencies(
            &self.source_plan,
            self.intent.budget().verification,
        )?;
        let paths = [
            &options.worktree,
            &options.checkpoint,
            &options.transfer.private_directory,
        ];
        let canonical: Vec<PathBuf> = paths
            .iter()
            .map(|path| {
                e2ee::FileRefMutationJournalProvider::validate_directory(
                    path,
                    self.secret.binding(),
                )
                .map_err(encrypted)?;
                std::fs::canonicalize(path).map_err(|error| {
                    encrypted(EncryptionError::CheckpointUnavailable(error.kind()))
                })
            })
            .collect::<Result<_>>()?;
        self.source
            .validate_encrypted_host_directories(&canonical)
            .map_err(encrypted)?;
        let retained = [
            &self.intent.journals.source,
            &self.intent.journals.target,
            &self.intent.journals.enrollment,
            &self.intent.journals.initialization,
            &self.intent.journals.progress,
        ]
        .iter()
        .map(|path| {
            std::fs::canonicalize(path)
                .map_err(|error| encrypted(EncryptionError::CheckpointUnavailable(error.kind())))
        })
        .collect::<Result<Vec<_>>>()?;
        for (index, path) in canonical.iter().enumerate() {
            if canonical[..index]
                .iter()
                .chain(retained.iter())
                .any(|other| path.starts_with(other) || other.starts_with(path))
            {
                return Err(encrypted(EncryptionError::InvalidSessionBinding));
            }
            if std::fs::read_dir(path)
                .map_err(|error| encrypted(EncryptionError::CheckpointUnavailable(error.kind())))?
                .next()
                .is_some()
            {
                return Err(invalid());
            }
        }
        let store = Arc::new(
            e2ee::FileCheckpointStore::create(&options.checkpoint, self.secret.binding())
                .map_err(encrypted)?,
        );
        let binding = SessionBinding::new(
            self.secret.binding().domain().to_owned(),
            self.secret.binding().account().to_owned(),
            self.secret.binding().repository(),
        )
        .map_err(encrypted)?;
        let secret = SecretSession::with_checkpoint_store(binding, store).map_err(encrypted)?;
        let credentials = RemoteCredentials::explicit();
        credentials
            .set_http_bearer_token(&self.intent.remote_name, options.bearer_token)
            .map_err(|_| encrypted(EncryptionError::InvalidSessionBinding))?;
        credentials
            .set_encrypted_session(
                &self.intent.remote_name,
                &self.intent.config,
                secret.clone(),
                options.transfer,
            )
            .map_err(|error| crate::repo_error(error.into()))?;
        let RemoteConfig::EncryptedHttp { url, .. } = &self.intent.config else {
            return Err(invalid());
        };
        let client = e2ee::EncryptedHttpClient::new(
            url,
            self.intent.remote_name.clone(),
            credentials.clone(),
            secret.clone(),
        )
        .map_err(http_error)?;
        run_http(client.unlock(password))?;
        let verification_operation = secret.begin_operation().map_err(encrypted)?;
        let remote = Arc::new(
            self.intent
                .config
                .clone()
                .build_with_credentials(&self.intent.remote_name, &credentials)
                .map_err(|error| crate::repo_error(error.into()))?,
        );
        graft::repo::Repository::verify_migration_target_references(&self.target_plan, &remote)
            .map_err(crate::repo_error)?;
        let repository =
            graft::repo::Repository::init(&options.worktree).map_err(crate::repo_error)?;
        let payload_bytes = repository
            .hydrate_migration_verification_objects(
                &self.source_plan,
                &remote,
                self.intent.max_payload_bytes,
            )
            .map_err(crate::repo_error)?;
        let dependencies = verify_storage(
            &self.source_plan,
            remote.clone(),
            self.intent.budget().verification,
            payload_bytes,
        )?;
        graft::repo::Repository::verify_migration_target_references(&self.target_plan, &remote)
            .map_err(crate::repo_error)?;
        run_http(client.verify())?;
        self.source
            .validate_migration_source_plan(&self.source_plan)?;
        verification_operation.check_active().map_err(encrypted)?;
        operation.check_active().map_err(encrypted)?;
        Ok(IndependentMigrationVerification {
            intent: digest(self.lease.snapshot().map_err(encrypted)?.intent()),
            references: self.target_plan.references().len(),
            repository_objects: self.source_plan.objects().len(),
            dependencies,
        })
    }

    /// Check the process-local proof's exact intent and current refs. A trusted
    /// host must hold its write fence across verification, this check and its
    /// own durable cutover; this method does not coordinate application writers.
    pub fn validate_independent_verification(
        &self,
        proof: &IndependentMigrationVerification,
    ) -> Result<()> {
        if proof.intent != digest(self.lease.snapshot().map_err(encrypted)?.intent()) {
            return Err(invalid());
        }
        self.source
            .validate_migration_source_plan(&self.source_plan)?;
        run_http(self.client.verify())?;
        graft::repo::Repository::verify_migration_target_references(
            &self.target_plan,
            &self.remote,
        )
        .map_err(crate::repo_error)?;
        self.source
            .validate_migration_source_plan(&self.source_plan)
    }

    /// Perform fresh independent recovery while the trusted host fence is held,
    /// switch host configuration durably and record confirmation. A crash after
    /// the host switch is recovered by replaying the same idempotent intent.
    pub fn verify_and_cutover(
        &mut self,
        fence: &mut impl MigrationWriteFence,
        password: zeroize::Zeroizing<Vec<u8>>,
        options: MigrationVerificationOptions,
    ) -> Result<IndependentMigrationVerification> {
        fence.check_held().map_err(encrypted)?;
        let repository = self.source.with_service(|service| {
            service
                .repository()
                .map_err(crate::repository_command_error)
        })?;
        if !fence.covers_repository(&repository) {
            return Err(encrypted(EncryptionError::InvalidSessionBinding));
        }
        let proof = self.verify_independently(password, options)?;
        fence.check_held().map_err(encrypted)?;
        self.validate_independent_verification(&proof)?;
        fence
            .commit_cutover(MigrationCutoverIntent {
                id: proof.intent,
                remote_name: &self.intent.remote_name,
                config: &self.intent.config,
                target: &self.target_plan,
            })
            .map_err(encrypted)?;
        fence.check_held().map_err(encrypted)?;
        let record = self.lease.snapshot().map_err(encrypted)?;
        if record.completed_steps() < record.total_steps() {
            self.lease
                .confirm_step(record.completed_steps())
                .map_err(encrypted)?;
        }
        Ok(proof)
    }
}

fn verify_storage(
    source: &MigrationSourcePlan,
    remote: Arc<Remote>,
    budget: MigrationVerificationBudget,
    payload_bytes: u64,
) -> Result<graft::repo::MigrationDependencyVerification> {
    use graft::core::lsn::LSNRangeExt;
    let runtime = graft::setup::setup_graft_temporary(RemoteConfig::Memory, None)
        .map_err(|error| crate::repository_command_error(error.into()))?;
    let mut expected = std::collections::BTreeMap::new();
    for snapshot in source.snapshots().values() {
        let mut target = graft::snapshot::Snapshot::empty();
        target.page_count = snapshot.page_count;
        for range in &snapshot.ranges {
            target.append(range.log.clone(), range.start..=range.end);
            let mut commits = range.commits.iter();
            for lsn in (range.start..=range.end).iter() {
                let commit = commits
                    .next()
                    .filter(|commit| commit.lsn == lsn)
                    .ok_or_else(invalid)?;
                let key = (range.log.clone(), lsn);
                if let Some(previous) = expected.get(&key) {
                    if previous != &commit.commit_hash {
                        return Err(invalid());
                    }
                } else {
                    if expected.len() >= budget.max_storage_commits {
                        return Err(encrypted(EncryptionError::TooLarge));
                    }
                    expected.insert(key, commit.commit_hash.clone());
                }
            }
            if commits.next().is_some() {
                return Err(invalid());
            }
        }
        runtime
            .snapshot_hydrate_from(target, remote.clone())
            .map_err(|error| crate::repository_command_error(error.into()))?;
        if let Some(head) = snapshot.ranges.first() {
            if runtime
                .get_commit(&head.log, head.end)
                .map_err(|error| crate::repository_command_error(error.into()))?
                .ok_or_else(invalid)?
                .page_count()
                != snapshot.page_count
            {
                return Err(invalid());
            }
        } else if !snapshot.page_count.is_empty() {
            return Err(invalid());
        }
    }
    let mut pages = 0u64;
    for ((log, lsn), hash) in &expected {
        let commit = runtime
            .get_commit(log, *lsn)
            .map_err(|error| crate::repository_command_error(error.into()))?
            .ok_or_else(invalid)?;
        pages = pages
            .checked_add(
                commit
                    .segment_idx()
                    .map_or(0, |index| u64::from(index.page_count().to_u32())),
            )
            .filter(|pages| *pages <= budget.max_storage_pages)
            .ok_or_else(|| encrypted(EncryptionError::TooLarge))?;
        if runtime
            .recompute_commit_hash(log, *lsn)
            .map_err(|error| crate::repository_command_error(error.into()))?
            .as_ref()
            != Some(hash)
        {
            return Err(invalid());
        }
    }
    Ok(graft::repo::MigrationDependencyVerification {
        payloads: source.external_payloads().len(),
        payload_bytes,
        snapshots: source.snapshots().len(),
        storage_commits: expected.len(),
        storage_pages: pages,
    })
}

fn http_error(error: e2ee::EncryptedHttpError) -> SdkError {
    SdkError::new(
        crate::encrypted_http_error_code(&error),
        "migration encrypted transport failed",
    )
}

fn validate_transport(
    source: &RepositorySession,
    intent: &RetainedIntent,
    remote: &Remote,
    config: &e2ee::EncryptedConfig,
) -> Result<()> {
    remote
        .authenticate_encrypted_recovery_config(config)
        .map_err(|error| crate::repo_error(error.into()))?;
    let staging = remote.encrypted_staging_directory().ok_or_else(invalid)?;
    source
        .validate_encrypted_host_directories(&[staging.to_owned()])
        .map_err(encrypted)?;
    let staging = std::fs::canonicalize(staging)
        .map_err(|error| encrypted(EncryptionError::CheckpointUnavailable(error.kind())))?;
    for path in [
        &intent.journals.source,
        &intent.journals.target,
        &intent.journals.enrollment,
        &intent.journals.initialization,
        &intent.journals.progress,
    ] {
        let path = std::fs::canonicalize(path)
            .map_err(|error| encrypted(EncryptionError::CheckpointUnavailable(error.kind())))?;
        if path.starts_with(&staging) || staging.starts_with(&path) {
            return Err(encrypted(EncryptionError::InvalidSessionBinding));
        }
    }
    Ok(())
}

fn run_http<T>(
    future: impl std::future::Future<Output = std::result::Result<T, e2ee::EncryptedHttpError>>,
) -> Result<T> {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    let runtime = match RUNTIME.get() {
        Some(runtime) => runtime,
        None => {
            let candidate = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("graft-migration-io")
                .enable_all()
                .build()
                .map_err(|error| encrypted(EncryptionError::CheckpointUnavailable(error.kind())))?;
            RUNTIME.get_or_init(|| candidate)
        }
    };
    runtime.block_on(future).map_err(http_error)
}

fn validate_intent(intent: &RetainedIntent, secret: &SecretSession) -> Result<()> {
    crate::validate_remote_name(&intent.remote_name)?;
    let RemoteConfig::EncryptedHttp { uuid, .. } = &intent.config else {
        return Err(invalid());
    };
    let repository = secret.binding().repository();
    let hex: String = repository
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let expected = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    if intent.version != 1
        || *uuid != expected
        || intent.max_storage_commits == 0
        || intent.max_storage_pages == 0
        || intent.max_storage_batch_pages == 0
        || intent.max_pack_bytes < 12
    {
        return Err(invalid());
    }
    Ok(())
}
