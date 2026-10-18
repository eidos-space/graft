//! Exact-intent migration, independent verification and fenced local-host cutover.
use super::*;
use graft::repo::{
    Head, MigrationPlanningBudget, MigrationPublicationBudget, MigrationRefSelection,
};
use graft_sdk::{
    EncryptedMigrationCoordinator, MigrationCoordinatorOptions, MigrationJournalPaths,
    MigrationPublicationState as State, MigrationVerificationOptions, RepositoryMigrationFence,
};

pub(super) fn is_action(action: &EncryptedAction) -> bool {
    matches!(
        action,
        EncryptedAction::PrepareMigration { .. }
            | EncryptedAction::ResumeMigration
            | EncryptedAction::VerifyMigration { .. }
            | EncryptedAction::CutoverMigration { .. }
    )
}

pub(super) fn preflight(args: &EncryptedArgs, source: &RepositorySession) -> Result<()> {
    if !is_action(&args.action) {
        return Ok(());
    }
    if matches!(args.action, EncryptedAction::PrepareMigration { .. }) != args.enroll {
        bail!(
            "prepare-migration requires --enroll; resume, verify and cutover reopen existing state"
        );
    }
    let repository = Repository::discover(&args.worktree)?;
    if let EncryptedAction::PrepareMigration {
        all_refs,
        references,
        mappings,
        target_head,
    } = &args.action
    {
        if !target_head.starts_with("refs/heads/") {
            bail!("migration target HEAD must be a full branch ref");
        }
        let source = repository.plan_migration_source(
            if *all_refs {
                MigrationRefSelection::All
            } else {
                MigrationRefSelection::Selected(references.clone())
            },
            MigrationPlanningBudget::default(),
        )?;
        repository.plan_migration_target_refs(
            &source,
            parse_mappings(mappings)?,
            Head::branch(
                target_head
                    .strip_prefix("refs/heads/")
                    .context("migration HEAD is not a branch")?,
            ),
        )?;
    }
    if let EncryptedAction::VerifyMigration { verification_directory }
    | EncryptedAction::CutoverMigration { verification_directory } = &args.action
    {
        if !verification_directory.is_absolute() {
            bail!("migration verification directory must be absolute");
        }
        source.validate_encrypted_host_directory_targets(std::slice::from_ref(
            verification_directory,
        ))?;
        RepositorySession::new(verification_directory)
            .validate_encrypted_host_directory_targets(std::slice::from_ref(&args.state_dir))?;
        if verification_directory.try_exists()?
            && fs::read_dir(verification_directory)?.next().is_some()
        {
            bail!("migration verification requires an empty or absent destination");
        }
    }
    Ok(())
}

fn parse_mappings(values: &[String]) -> Result<Vec<(String, String)>> {
    values
        .iter()
        .map(|value| {
            let (source, target) = value
                .split_once('=')
                .context("migration mapping must be SOURCE_FULL_REF=TARGET_FULL_REF")?;
            if source.is_empty() || target.is_empty() {
                bail!("migration mapping names cannot be empty");
            }
            Ok((source.to_owned(), target.to_owned()))
        })
        .collect()
}

fn paths(state: &Path) -> MigrationJournalPaths {
    MigrationJournalPaths {
        source: state.join("migration-source"),
        target: state.join("migration-target"),
        enrollment: state.join("migration-enrollment"),
        initialization: state.join("migration-initialization"),
        progress: state.join("migration-progress"),
    }
}

pub(super) fn run(
    args: &EncryptedArgs,
    secret: &SecretSession,
    credentials: RemoteCredentials,
    runtime: &tokio::runtime::Runtime,
    recovery: (
        Arc<FileRefMutationJournalProvider>,
        Arc<FileMultipartJournalProvider>,
    ),
    password: Zeroizing<Vec<u8>>,
) -> Result<Value> {
    let remote = RemoteConfig::EncryptedHttp {
        url: args.url.clone(),
        uuid: args.uuid.clone(),
    };
    let source = Arc::new(RepositorySession::new_with_remote_credentials(
        &args.worktree,
        None,
        credentials.clone(),
    ));
    source.open()?;
    let outcome: Result<Value> = (|| {
        let paths = paths(&args.state_dir);
        let create = matches!(args.action, EncryptedAction::PrepareMigration { .. });
        if create {
            for path in [
                &paths.source,
                &paths.target,
                &paths.enrollment,
                &paths.initialization,
                &paths.progress,
            ] {
                source.validate_encrypted_host_directory_targets(std::slice::from_ref(path))?;
                create_private(path)?;
            }
        }
        let verification_password = Zeroizing::new(password.to_vec());
        let initial = runtime.block_on(secret.prepare_native_initial_config(
            password,
            paths.initialization.clone(),
            create,
        )?)?;
        credentials.set_encrypted_session_with_recovery(
            &args.remote,
            &remote,
            secret.clone(),
            EncryptedTransferBudget {
                private_directory: args.state_dir.join("staging"),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 8 * 1024 * 1024 * 1024,
            },
            recovery.0,
            recovery.1,
        )?;
        let mut coordinator = if let EncryptedAction::PrepareMigration {
            all_refs,
            references,
            mappings,
            target_head,
        } = &args.action
        {
            let plan = source.plan_migration_source(
                if *all_refs {
                    MigrationRefSelection::All
                } else {
                    MigrationRefSelection::Selected(references.clone())
                },
                MigrationPlanningBudget::default(),
            )?;
            let target = source.plan_migration_target_refs(
                &plan,
                parse_mappings(mappings)?,
                Head::branch(
                    target_head
                        .strip_prefix("refs/heads/")
                        .context("migration HEAD is not a branch")?,
                ),
            )?;
            source.create_migration_source_journal(&paths.source, secret.binding(), &plan)?;
            source.create_migration_target_journal(&paths.target, secret.binding(), &target)?;
            source.create_migration_enrollment_journal(
                &paths.enrollment,
                &paths.target,
                secret,
                initial.as_ref(),
            )?;
            EncryptedMigrationCoordinator::prepare(
                source.clone(),
                secret.clone(),
                credentials,
                MigrationCoordinatorOptions {
                    remote_name: args.remote.clone(),
                    config: remote,
                    journals: paths,
                    budget: MigrationPublicationBudget::default(),
                },
            )?
        } else {
            EncryptedMigrationCoordinator::resume(
                source.clone(),
                secret.clone(),
                credentials,
                &paths.progress,
            )?
        };
        match &args.action {
            EncryptedAction::PrepareMigration { .. } => {
                Ok(status("prepare-migration", coordinator.state()?))
            }
            EncryptedAction::ResumeMigration => {
                while !matches!(
                    coordinator.state()?,
                    State::ReadyForIndependentVerification | State::HostCutoverConfirmed
                ) {
                    coordinator.publish_next()?;
                }
                Ok(status("resume-migration", coordinator.state()?))
            }
            EncryptedAction::VerifyMigration { verification_directory }
            | EncryptedAction::CutoverMigration { verification_directory } => {
                // Acquire the canonical repository mutation gate before creating verification
                // state and retain it through revalidation and the durable config switch.
                let mut fence = matches!(args.action, EncryptedAction::CutoverMigration { .. })
                    .then(|| RepositoryMigrationFence::acquire(&source))
                    .transpose()?;
                if !verification_directory.try_exists()? {
                    create_private(verification_directory)?;
                }
                let temporary = tempfile::Builder::new()
                    .prefix("migration-verification-")
                    .tempdir_in(&args.state_dir)?;
                fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
                let checkpoint = temporary.path().join("checkpoint");
                let staging = temporary.path().join("staging");
                create_private(&checkpoint)?;
                create_private(&staging)?;
                let bearer_token = Zeroizing::new(
                    std::env::var(&args.token_env)
                        .context("account bearer token is unavailable")?,
                );
                let verification = MigrationVerificationOptions {
                    worktree: verification_directory.clone(),
                    checkpoint,
                    transfer: EncryptedTransferBudget {
                        private_directory: staging,
                        max_buffered_plaintext: 4 * 1024 * 1024,
                        max_ciphertext_spool: 8 * 1024 * 1024 * 1024,
                    },
                    bearer_token: bearer_token.to_string(),
                };
                let proof = if let Some(fence) = fence.as_mut() {
                    coordinator.verify_and_cutover(fence, verification_password, verification)?
                } else {
                    coordinator.verify_independently(verification_password, verification)?
                };
                let cutover = fence.is_some();
                Ok(
                    serde_json::json!({"operation":if cutover {"cutover-migration"} else {"verify-migration"}, "status":if cutover {"host-cutover-confirmed"} else {"independently-verified"}, "host_cutover":if cutover {"confirmed"} else {"pending-trusted-writer-fence"}, "offline_devices":"pending", "references":proof.references, "repository_objects":proof.repository_objects, "dependencies":proof.dependencies}),
                )
            }
            _ => unreachable!("only migration actions are dispatched here"),
        }
    })();
    let closed = source.close();
    let value = outcome?;
    closed?;
    Ok(value)
}

fn status(operation: &str, state: State) -> Value {
    let phase = match state {
        State::Prepared => "prepared",
        State::ConfigConfirmed => "config-confirmed",
        State::DependenciesConfirmed => "dependencies-confirmed",
        State::PublishingRefs { .. } => "publishing-refs",
        State::ReadyForIndependentVerification => "ready-for-independent-verification",
        State::HostCutoverConfirmed => "host-cutover-confirmed",
    };
    serde_json::json!({"operation":operation,"status":phase,"host_cutover":if matches!(state,State::HostCutoverConfirmed) {"confirmed"} else {"pending-trusted-writer-fence"},"offline_devices":"pending"})
}
