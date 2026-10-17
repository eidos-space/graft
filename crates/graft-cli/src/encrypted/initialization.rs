//! Source-bound native initialization. Configuration confirmation alone is pending.
use super::*;
use graft::e2ee::{
    InitialConfigPublicationOutcome, InitializationIntent, InitializationJournal,
    InitializationStage as Stage,
};

fn verification_directory(args: &EncryptedArgs) -> Option<&Path> {
    match &args.action {
        EncryptedAction::Initialize { verification_directory }
        | EncryptedAction::ResumeInitialization { verification_directory } => {
            Some(verification_directory)
        }
        _ => None,
    }
}

pub(super) fn preflight(args: &EncryptedArgs, source: &RepositorySession) -> Result<()> {
    let Some(directory) = verification_directory(args) else {
        return Ok(());
    };
    if !directory.is_absolute() {
        bail!("initialization verification directory must be absolute");
    }
    if matches!(args.action, EncryptedAction::Initialize { .. }) != args.enroll {
        bail!("initialize requires --enroll; resume-initialization must reopen existing state");
    }
    source.validate_encrypted_host_directory_targets(&[directory.to_owned()])?;
    RepositorySession::new(directory)
        .validate_encrypted_host_directory_targets(std::slice::from_ref(&args.state_dir))?;
    if directory.try_exists()? && fs::read_dir(directory)?.next().is_some() {
        bail!("initialization verification requires an empty or absent destination");
    }
    Ok(())
}

pub(super) fn run(
    args: &EncryptedArgs,
    client: &EncryptedHttpClient,
    secret: &SecretSession,
    credentials: RemoteCredentials,
    runtime: &tokio::runtime::Runtime,
    recovery: (
        Arc<FileRefMutationJournalProvider>,
        Arc<FileMultipartJournalProvider>,
    ),
    password: Zeroizing<Vec<u8>>,
) -> Result<Value> {
    let (journals, uploads) = recovery;
    let directory =
        verification_directory(args).context("initialization destination is unavailable")?;
    let remote = RemoteConfig::EncryptedHttp {
        url: args.url.clone(),
        uuid: args.uuid.clone(),
    };
    let session =
        RepositorySession::new_with_remote_credentials(&args.worktree, None, credentials.clone());
    session.open()?;
    let result: Result<Value> = (|| {
        let source = session.repository_metadata()?;
        if source.current_branch.as_deref() != Some(args.branch.as_str()) {
            return Err(graft::e2ee::EncryptionError::StaleObservation.into());
        }
        let intent = InitializationIntent::new(
            args.branch.clone(),
            source
                .current_head
                .context("initialization requires a committed source")?,
        )?;
        if matches!(args.action, EncryptedAction::Initialize { .. })
            && !Repository::discover(&args.worktree)?
                .config()?
                .remotes
                .contains_key(&args.remote)
        {
            session.add_remote_with_config(&args.remote, remote.clone())?;
        }
        session.validate_initialization_remote_config(&args.remote, &remote)?;
        let recovery = args.state_dir.join("initialization");
        let create = matches!(args.action, EncryptedAction::Initialize { .. });
        if create {
            create_private(&recovery)?;
        }
        session.validate_encrypted_host_directories(std::slice::from_ref(&recovery))?;
        let verification_password = Zeroizing::new(password.to_vec());
        let preparation = secret.prepare_native_initial_config_with_intent(
            password,
            recovery,
            create,
            intent.clone(),
        )?;
        let journal = runtime.block_on(preparation)?;
        // Synchronous journal work owns this admission through all source checks
        // and stage persistence; disconnect cannot finish while it remains alive.
        let operation = secret.begin_operation()?;
        let record = journal.load(secret.binding())?;
        operation.authenticate_retained_config(&record.config)?;
        let retained = record
            .intent
            .context("retained initialization source is unavailable")?;
        retained.require_source(intent.branch(), intent.commit())?;
        session.validate_initialization_remote_config(&args.remote, &remote)?;
        let current = session.repository_metadata()?;
        retained.require_source(
            current
                .current_branch
                .as_deref()
                .context("initialization source branch changed")?,
            current
                .current_head
                .as_deref()
                .context("initialization source commit changed")?,
        )?;
        if matches!(record.stage, Stage::Prepared | Stage::ConfigPublished) {
            let outcome = if create {
                runtime.block_on(client.publish_initial_config(journal.clone()))?
            } else {
                let outcome = runtime.block_on(client.reconcile_initial_config(journal.clone()))?;
                if matches!(outcome, InitialConfigPublicationOutcome::Unconfirmed) {
                    runtime.block_on(client.publish_initial_config(journal.clone()))?
                } else {
                    outcome
                }
            };
            match outcome {
                InitialConfigPublicationOutcome::Confirmed => {}
                InitialConfigPublicationOutcome::Unconfirmed => {
                    return Err(ConfigPublicationError(
                        graft_sdk::SdkErrorCode::RemotePublicationUnconfirmed,
                    )
                    .into());
                }
                InitialConfigPublicationOutcome::Conflict => {
                    return Err(
                        ConfigPublicationError(graft_sdk::SdkErrorCode::RepositoryStale).into(),
                    );
                }
                InitialConfigPublicationOutcome::OutcomeUnknown { .. } => {
                    return Err(ConfigPublicationError(
                        graft_sdk::SdkErrorCode::RemotePublicationOutcomeUnknown,
                    )
                    .into());
                }
            }
        }
        operation.check_active()?;
        credentials.set_encrypted_session_with_recovery(
            &args.remote,
            &remote,
            secret.clone(),
            EncryptedTransferBudget {
                private_directory: args.state_dir.join("staging"),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 8 * 1024 * 1024 * 1024,
            },
            journals,
            uploads,
        )?;
        // The command service rejects source changes before discovery and again
        // before publication, and refuses competing refs including ancestors.
        session.push_initialization_intent_with_config(&args.remote, &remote, &retained)?;
        verify(
            args,
            directory,
            &remote,
            verification_password,
            runtime,
            &retained,
        )?;
        for (expected, next) in [
            (Stage::ConfigPublished, Stage::ObjectsPublished),
            (Stage::ObjectsPublished, Stage::RefPublished),
            (Stage::RefPublished, Stage::Connected),
        ] {
            operation.check_active()?;
            if journal.load(secret.binding())?.stage == expected {
                journal.advance(secret.binding(), expected, next)?;
            }
        }
        operation.check_active()?;
        if journal.load(secret.binding())?.stage != Stage::Connected {
            return Err(graft::e2ee::EncryptionError::StaleObservation.into());
        }
        Ok(serde_json::json!({"operation":"initialize","status":"connected"}))
    })();
    let closed = session.close();
    let value = result?;
    closed?;
    Ok(value)
}

fn verify(
    args: &EncryptedArgs,
    directory: &Path,
    remote: &RemoteConfig,
    password: Zeroizing<Vec<u8>>,
    runtime: &tokio::runtime::Runtime,
    intent: &InitializationIntent,
) -> Result<()> {
    let state = tempfile::Builder::new()
        .prefix("initialization-verification-")
        .tempdir_in(&args.state_dir)?;
    let checkpoint = state.path().join("checkpoint");
    let staging = state.path().join("staging");
    for path in [&checkpoint, &staging] {
        create_private(path)?;
    }
    let binding = SessionBinding::new(
        format!("graft-cli:{}", args.url),
        args.account.clone(),
        repository_id(&args.uuid)?,
    )?;
    let store = FileCheckpointStore::create(&checkpoint, &binding)?;
    let secret = SecretSession::with_checkpoint_store(binding, Arc::new(store))?;
    let credentials = RemoteCredentials::explicit();
    let token = Zeroizing::new(
        std::env::var(&args.token_env)
            .map_err(|_| anyhow::anyhow!("account bearer token is unavailable"))?,
    );
    credentials.set_http_bearer_token("origin", token.to_string())?;
    let client = EncryptedHttpClient::new(
        &args.url,
        "origin".into(),
        credentials.clone(),
        secret.clone(),
    )?;
    let result: Result<()> = (|| {
        runtime.block_on(client.unlock(password))?;
        credentials.set_encrypted_session(
            "origin",
            remote,
            secret.clone(),
            EncryptedTransferBudget {
                private_directory: staging.clone(),
                max_buffered_plaintext: 4 * 1024 * 1024,
                max_ciphertext_spool: 8 * 1024 * 1024 * 1024,
            },
        )?;
        let verifier = RepositorySession::new_with_remote_credentials(directory, None, credentials);
        verifier.validate_encrypted_host_directories(&[checkpoint, staging])?;
        verifier.open()?;
        let verified: Result<()> = (|| {
            verifier.clone_repository_with_config(remote.clone(), None)?;
            let actual = verifier.repository_metadata()?;
            if actual.current_head.as_deref() != Some(intent.commit())
                || actual.current_branch.as_deref() != Some(intent.branch())
            {
                return Err(graft::e2ee::EncryptionError::StaleObservation.into());
            }
            Ok(())
        })();
        let closed = verifier.close();
        verified?;
        closed?;
        Ok(())
    })();
    runtime.block_on(secret.disconnect());
    result
}
