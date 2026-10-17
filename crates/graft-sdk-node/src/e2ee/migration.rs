//! Trusted Node host migration operations. Capabilities never cross into JS.
use super::*;
use graft_sdk::{
    EncryptedMigrationCoordinator, Head, MigrationCoordinatorOptions, MigrationJournalPaths,
    MigrationPlanningBudget, MigrationPublicationBudget, MigrationPublicationState as State,
    MigrationRefSelection, MigrationVerificationOptions, RepositoryMigrationFence,
    RepositorySession,
};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
};

#[napi(object)]
pub struct MigrationRefMapping {
    pub source: String,
    pub target: String,
}

#[napi(object)]
pub struct PrepareEncryptedMigrationOptions {
    /// Exactly one of `all_refs=true` or a nonempty references list is required.
    pub all_refs: Option<bool>,
    pub references: Option<Vec<String>>,
    pub mappings: Vec<MigrationRefMapping>,
    pub target_head: String,
}

#[napi(object)]
pub struct VerifyEncryptedMigrationOptions {
    /// Empty or absent destination outside source and all host state.
    pub verification_directory: String,
    /// Fresh account token for the same host-verified principal.
    pub bearer_token: String,
}

pub(crate) enum Action {
    Prepare(PrepareEncryptedMigrationOptions),
    Resume,
    Verify(VerifyEncryptedMigrationOptions),
    Cutover(VerifyEncryptedMigrationOptions),
}

fn paths(root: &Path) -> MigrationJournalPaths {
    MigrationJournalPaths {
        source: root.join("migration-source"),
        target: root.join("migration-target"),
        enrollment: root.join("migration-enrollment"),
        initialization: root.join("migration-initialization"),
        progress: root.join("migration-progress"),
    }
}

fn private_directory(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(io_error)?;
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    fs::File::open(
        path.parent()
            .ok_or_else(|| secret_error(EncryptionError::InvalidEncoding))?,
    )
    .and_then(|file| file.sync_all())
    .map_err(io_error)
}

fn io_error(error: std::io::Error) -> Error {
    secret_error(EncryptionError::CheckpointUnavailable(error.kind()))
}

/// Password copying/wiping and unlock admission happen before returning a promise.
pub(crate) fn run(
    env: Env,
    source: Arc<RepositorySession>,
    authority: &NodeEncryptedRemoteSession,
    mut password: Buffer,
    action: Action,
) -> Result<AsyncBlock<String>> {
    let valid =
        !password.is_empty() && password.len() <= 1024 && std::str::from_utf8(&password).is_ok();
    if !valid {
        password.fill(0);
        return Err(secret_error(EncryptionError::InvalidPassword));
    }
    let input = Zeroizing::new(password.to_vec());
    password.fill(0);
    source
        .validate_encrypted_host_directories(&authority.host_directories)
        .map_err(secret_error)?;
    if let Action::Verify(options) | Action::Cutover(options) = &action {
        validate_verification_directory(
            &source,
            &authority.host_directories,
            &options.verification_directory,
        )?;
    }
    let create = matches!(action, Action::Prepare(_));
    let create_cutover = matches!(action, Action::Cutover(_));
    let secret = authority.secret.clone();
    let verification_password = Zeroizing::new(input.to_vec());
    let recovery = authority.host_directories[1].clone();
    let paths = paths(&recovery);
    let future = secret
        .prepare_native_initial_config(input, paths.initialization.clone(), create)
        .map_err(secret_error)?;
    let config = authority.config.clone();
    let credentials = authority.credentials.clone();
    let remote_name = authority.remote_name.clone();
    let budget = authority.budget.clone();
    let host_directories = authority.host_directories.clone();
    AsyncBlockBuilder::new(async move {
        let result = async {
            let prepared = if let Action::Prepare(options) = &action {
                let source = source.clone();
                let paths = paths.clone();
                let selection = match (options.all_refs.unwrap_or(false), &options.references) {
                    (true, None) => MigrationRefSelection::All,
                    (false, Some(names)) if !names.is_empty() => MigrationRefSelection::Selected(names.clone()),
                    _ => return Err(secret_error(EncryptionError::InvalidEncoding)),
                };
                let head = options.target_head.strip_prefix("refs/heads/")
                    .ok_or_else(|| secret_error(EncryptionError::InvalidEncoding))?.to_owned();
                let mappings = options.mappings.iter().map(|entry| (entry.source.clone(), entry.target.clone())).collect();
                Some(tokio::task::spawn_blocking(move || {
                    let plan = source.plan_migration_source(selection, MigrationPlanningBudget::default()).map_err(crate::napi_error)?;
                    let target = source.plan_migration_target_refs(&plan, mappings, Head::branch(head)).map_err(crate::napi_error)?;
                    // Validate selection/mapping before allocating any migration state.
                    for path in [&paths.source, &paths.target, &paths.enrollment, &paths.initialization, &paths.progress] {
                        source.validate_encrypted_host_directory_targets(std::slice::from_ref(path)).map_err(secret_error)?;
                        if path.try_exists().map_err(io_error)? { return Err(secret_error(EncryptionError::StaleObservation)); }
                    }
                    for path in [&paths.source, &paths.target, &paths.enrollment, &paths.initialization, &paths.progress] { private_directory(path)?; }
                    Ok::<_, Error>((plan, target))
                }).await.map_err(|_| secret_error(EncryptionError::Cancelled))??)
            } else { None };
            let initial = future.await.map_err(secret_error)?;
            let admission = secret.begin_operation().map_err(secret_error)?;
            let task_secret = secret.clone();
            tokio::task::spawn_blocking(move || {
                admission.check_active().map_err(secret_error)?;
                let mut coordinator = if let Some((plan, target)) = prepared {
                    source.create_migration_source_journal(&paths.source, task_secret.binding(), &plan).map_err(crate::napi_error)?;
                    source.create_migration_target_journal(&paths.target, task_secret.binding(), &target).map_err(crate::napi_error)?;
                    source.create_migration_enrollment_journal(&paths.enrollment, &paths.target, &task_secret, initial.as_ref()).map_err(crate::napi_error)?;
                    EncryptedMigrationCoordinator::prepare(source.clone(), task_secret, credentials,
                        MigrationCoordinatorOptions { remote_name, config, journals: paths, budget: MigrationPublicationBudget::default() })
                } else {
                    EncryptedMigrationCoordinator::resume(source.clone(), task_secret, credentials, &paths.progress)
                }.map_err(crate::napi_error)?;
                let value = match action {
                    Action::Prepare(_) => status(coordinator.state().map_err(crate::napi_error)?),
                    Action::Resume => {
                        while !matches!(coordinator.state().map_err(crate::napi_error)?, State::ReadyForIndependentVerification | State::HostCutoverConfirmed) {
                            admission.check_active().map_err(secret_error)?;
                            coordinator.publish_next().map_err(crate::napi_error)?;
                        }
                        status(coordinator.state().map_err(crate::napi_error)?)
                    }
                    Action::Verify(options) | Action::Cutover(options) => {
                        let mut fence = if create_cutover { Some(RepositoryMigrationFence::acquire(&source).map_err(crate::napi_error)?) } else { None };
                        validate_verification_directory(&source, &host_directories, &options.verification_directory)?;
                        let worktree = PathBuf::from(options.verification_directory);
                        if !worktree.try_exists().map_err(io_error)? { private_directory(&worktree)?; }
                        let temporary = tempfile::Builder::new().prefix("migration-verifier-").tempdir_in(&recovery).map_err(io_error)?;
                        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).map_err(io_error)?;
                        let checkpoint = temporary.path().join("checkpoint");
                        let staging = temporary.path().join("staging");
                        private_directory(&checkpoint)?;
                        private_directory(&staging)?;
                        let options = MigrationVerificationOptions { worktree, checkpoint,
                            transfer: EncryptedTransferBudget { private_directory: staging, ..budget }, bearer_token: options.bearer_token };
                        let proof = if let Some(fence) = fence.as_mut() {
                            coordinator.verify_and_cutover(fence, verification_password, options)
                        } else { coordinator.verify_independently(verification_password, options) }.map_err(crate::napi_error)?;
                        serde_json::json!({"status":if create_cutover {"host-cutover-confirmed"} else {"independently-verified"},
                            "hostCutover":if create_cutover {"confirmed"} else {"pending"}, "offlineDevices":"pending",
                            "references":proof.references,"repositoryObjects":proof.repository_objects,"dependencies":proof.dependencies})
                    }
                };
                admission.check_active().map_err(secret_error)?;
                serde_json::to_string(&value).map_err(|_| secret_error(EncryptionError::InvalidEncoding))
            }).await.map_err(|_| secret_error(EncryptionError::Cancelled))?
        }.await;
        // Every operation owns its temporary unlock. Completion includes drain.
        secret.lock().await;
        result
    }).build(&env)
}

fn status(state: State) -> serde_json::Value {
    let phase = match state {
        State::Prepared => "prepared",
        State::ConfigConfirmed => "config-confirmed",
        State::DependenciesConfirmed => "dependencies-confirmed",
        State::PublishingRefs { .. } => "publishing-refs",
        State::ReadyForIndependentVerification => "ready-for-independent-verification",
        State::HostCutoverConfirmed => "host-cutover-confirmed",
    };
    serde_json::json!({"status":phase,"hostCutover":if matches!(state,State::HostCutoverConfirmed) {"confirmed"} else {"pending"},"offlineDevices":"pending"})
}
