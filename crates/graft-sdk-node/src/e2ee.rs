//! Trusted host API. Never expose these handles to ordinary plugin RPC.
use graft_sdk::e2ee::{
    EncryptedHttpClient, EncryptionError, RememberedKeyProvider, RepositoryId, SecretSession,
    SecretSessionState, SessionBinding,
};
use graft_sdk::{EncryptedTransferBudget, RemoteConfig, RemoteCredentials};
use napi::{
    Env, Error, Result, Status,
    bindgen_prelude::{AsyncBlock, AsyncBlockBuilder, Buffer},
};
use napi_derive::napi;
use std::{path::PathBuf, sync::Arc};
use zeroize::Zeroizing;

#[cfg(unix)]
pub(super) mod migration;

#[napi(object)]
pub struct EncryptedRemoteSessionOptions {
    pub remote_name: String,
    pub url: String,
    pub uuid: String,
    /// Identity verified by the trusted host against its authenticated account.
    pub authentication_domain: String,
    pub account_subject: String,
    pub bearer_token: String,
    pub checkpoint_directory: String,
    /// Explicit new-device enrollment; never recreate a missing existing record.
    pub create_checkpoint: bool,
    pub staging_directory: String,
    /// Durable owner-only directory outside repository and disposable staging.
    pub recovery_directory: String,
    pub keychain_application: Option<String>,
    pub keychain_access_group: Option<String>,
}

#[napi(js_name = "EncryptedRemoteSession")]
pub struct NodeEncryptedRemoteSession {
    pub(super) credentials: RemoteCredentials,
    pub(super) config: RemoteConfig,
    pub(super) remote_name: String,
    secret: SecretSession,
    client: EncryptedHttpClient,
    budget: EncryptedTransferBudget,
    provider: Option<Arc<dyn RememberedKeyProvider>>,
    journals: Arc<dyn graft_sdk::e2ee::RefMutationJournalProvider>,
    multipart_journals: Arc<dyn graft_sdk::e2ee::MultipartJournalProvider>,
    config_journal: Arc<dyn graft_sdk::e2ee::ConfigMutationJournal>,
    pub(super) host_directories: [PathBuf; 3],
}

pub(super) struct InitialBranchContext {
    operation: graft_sdk::e2ee::SessionOperation,
    secret: SecretSession,
    directory: PathBuf,
    remote: String,
    credentials: RemoteCredentials,
    config: RemoteConfig,
    host_directories: [PathBuf; 3],
}

impl NodeEncryptedRemoteSession {
    pub(super) fn initial_branch_context(&self) -> Result<Arc<InitialBranchContext>> {
        Ok(Arc::new(InitialBranchContext {
            operation: self.secret.begin_operation().map_err(secret_error)?,
            secret: self.secret.clone(),
            directory: self.host_directories[1].clone(),
            remote: self.remote_name.clone(),
            credentials: self.credentials.clone(),
            config: self.config.clone(),
            host_directories: self.host_directories.clone(),
        }))
    }
}

pub(super) fn validate_verification_directory(
    source: &graft_sdk::RepositorySession,
    host_directories: &[PathBuf],
    directory: &str,
) -> Result<()> {
    source
        .validate_encrypted_host_directory_targets(&[PathBuf::from(directory)])
        .map_err(secret_error)?;
    let verifier = graft_sdk::RepositorySession::new(directory);
    verifier
        .validate_encrypted_host_directories(host_directories)
        .map_err(secret_error)?;
    verifier
        .validate_clone_destination()
        .map_err(super::napi_error)
}

pub(super) fn push_initial_branch(
    session: &graft_sdk::RepositorySession,
    context: &InitialBranchContext,
    branch: Option<&str>,
    verification_directory: Option<&str>,
) -> Result<serde_json::Value> {
    #[cfg(unix)]
    {
        use graft_sdk::e2ee::{
            FileInitializationJournal, InitializationIntent, InitializationJournal,
            InitializationStage,
        };
        context.operation.check_active().map_err(secret_error)?;
        if let Some(directory) = verification_directory {
            // Repeat inside the queued operation before intent or remote effects.
            validate_verification_directory(session, &context.host_directories, directory)?;
        }
        session
            .validate_initialization_remote_config(&context.remote, &context.config)
            .map_err(super::napi_error)?;
        let journal = FileInitializationJournal::open(&context.directory, context.secret.binding())
            .map_err(secret_error)?;
        let record = journal
            .load(context.secret.binding())
            .map_err(secret_error)?;
        context
            .operation
            .authenticate_retained_config(&record.config)
            .map_err(secret_error)?;
        if record.config.generation() != 1 || record.stage == InitializationStage::Prepared {
            return Err(secret_error(EncryptionError::StaleObservation));
        }
        let source = session.repository_metadata().map_err(super::napi_error)?;
        let selected = branch
            .or(source.current_branch.as_deref())
            .ok_or_else(|| secret_error(EncryptionError::InvalidEncoding))?;
        // Only the currently checked-out committed source is selected here.
        // Resumes load their existing intent; they never reconstruct from HEAD.
        if source.current_branch.as_deref() != Some(selected) {
            return Err(secret_error(EncryptionError::StaleObservation));
        }
        let head = source
            .current_head
            .as_deref()
            .ok_or_else(|| secret_error(EncryptionError::InvalidEncoding))?;
        let intent = if let Some(intent) = record.intent {
            intent
                .require_source(selected, head)
                .map_err(secret_error)?;
            intent
        } else {
            if record.stage != InitializationStage::ConfigPublished {
                return Err(secret_error(EncryptionError::StaleObservation));
            }
            let intent =
                InitializationIntent::new(selected.into(), head.into()).map_err(secret_error)?;
            journal
                .bind_intent(context.secret.binding(), &intent)
                .map_err(secret_error)?;
            intent
        };
        context.operation.check_active().map_err(secret_error)?;
        // The repository service rechecks source after remote discovery and
        // before dependencies. Failed publication leaves the same durable intent.
        let pushed = session
            .push_initialization_intent_with_config(&context.remote, &context.config, &intent)
            .map_err(super::napi_error)?;
        if let Some(directory) = verification_directory {
            context.operation.check_active().map_err(secret_error)?;
            validate_verification_directory(session, &context.host_directories, directory)?;
            let verifier = graft_sdk::RepositorySession::new_with_remote_credentials(
                directory,
                None,
                context
                    .credentials
                    .for_encrypted_remote_alias(&context.remote, "origin")
                    .map_err(|_| secret_error(EncryptionError::InvalidSessionBinding))?,
            );
            verifier
                .validate_encrypted_host_directories(&context.host_directories)
                .map_err(secret_error)?;
            verifier.open().map_err(super::napi_error)?;
            let verified = (|| {
                verifier
                    .clone_repository_with_config(context.config.clone(), None)
                    .map_err(super::napi_error)?;
                let actual = verifier.repository_metadata().map_err(super::napi_error)?;
                if actual.current_head.as_deref() != Some(intent.commit())
                    || actual.current_branch.as_deref() != Some(intent.branch())
                {
                    return Err(secret_error(EncryptionError::StaleObservation));
                }
                Ok(())
            })();
            let closed = verifier.close().map_err(super::napi_error);
            verified?;
            closed?;
            for (expected, next) in [
                (
                    InitializationStage::ConfigPublished,
                    InitializationStage::ObjectsPublished,
                ),
                (
                    InitializationStage::ObjectsPublished,
                    InitializationStage::RefPublished,
                ),
                (
                    InitializationStage::RefPublished,
                    InitializationStage::Connected,
                ),
            ] {
                context.operation.check_active().map_err(secret_error)?;
                let current = journal
                    .load(context.secret.binding())
                    .map_err(secret_error)?
                    .stage;
                if current == expected {
                    journal
                        .advance(context.secret.binding(), expected, next)
                        .map_err(secret_error)?;
                }
            }
            context.operation.check_active().map_err(secret_error)?;
            if journal
                .load(context.secret.binding())
                .map_err(secret_error)?
                .stage
                != InitializationStage::Connected
            {
                return Err(secret_error(EncryptionError::StaleObservation));
            }
            Ok(serde_json::json!({"state": "connected", "push": pushed}))
        } else {
            Ok(serde_json::json!({"state": "branch_published", "push": pushed}))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (session, context, branch, verification_directory);
        Err(secret_error(EncryptionError::SecureStorageUnavailable))
    }
}

#[napi]
impl NodeEncryptedRemoteSession {
    #[napi(constructor)]
    pub fn new(options: EncryptedRemoteSessionOptions) -> Result<Self> {
        let repository = parse_uuid(&options.uuid)?;
        let binding = SessionBinding::new(
            options.authentication_domain,
            options.account_subject,
            repository,
        )
        .map_err(secret_error)?;
        let provider = key_provider(options.keychain_application, options.keychain_access_group)?;
        let credentials = RemoteCredentials::explicit();
        credentials
            .set_http_bearer_token(&options.remote_name, options.bearer_token)
            .map_err(|_| error("invalid_credential", "invalid remote credential"))?;
        // Validate the endpoint before creating durable enrollment. This temporary
        // locked session never has a key, provider, checkpoint or network operation.
        let probe = SecretSession::new(
            SessionBinding::new(
                binding.domain().into(),
                binding.account().into(),
                repository,
            )
            .map_err(secret_error)?,
            None,
        )
        .map_err(secret_error)?;
        EncryptedHttpClient::new(
            &options.url,
            options.remote_name.clone(),
            credentials.clone(),
            probe,
        )
        .map_err(http_error)?;
        let budget = EncryptedTransferBudget {
            private_directory: PathBuf::from(&options.staging_directory),
            max_buffered_plaintext: 64 * 1024 * 1024,
            max_ciphertext_spool: 1024 * 1024 * 1024,
        };
        validate_private_directory(&budget.private_directory)?;
        let canonical = |path: &std::path::Path| {
            std::fs::canonicalize(path)
                .map_err(|error| secret_error(EncryptionError::CheckpointUnavailable(error.kind())))
        };
        let staging = canonical(&budget.private_directory)?;
        if canonical(std::path::Path::new(&options.recovery_directory))?.starts_with(&staging)
            || canonical(std::path::Path::new(&options.checkpoint_directory))?.starts_with(&staging)
        {
            return Err(secret_error(EncryptionError::InvalidSessionBinding));
        }
        #[cfg(unix)]
        graft_sdk::e2ee::FileRefMutationJournalProvider::validate_directory(
            &options.recovery_directory,
            &binding,
        )
        .map_err(secret_error)?;
        // Claim the checkpoint first: failed new enrollment must never recreate
        // lost recovery storage alongside an already enrolled checkpoint.
        let store = checkpoint_store(
            &options.checkpoint_directory,
            &binding,
            options.create_checkpoint,
        )?;
        let journals = recovery_provider(
            &options.recovery_directory,
            &binding,
            options.create_checkpoint,
        )?;
        let config_journal = config_journal(
            &options.recovery_directory,
            &binding,
            options.create_checkpoint,
        )?;
        let multipart_journals = multipart_provider(
            &options.recovery_directory,
            &binding,
            options.create_checkpoint,
        )?;
        let secret = SecretSession::with_checkpoint_store(binding, store).map_err(secret_error)?;
        let config = RemoteConfig::EncryptedHttp {
            url: options.url.clone(),
            uuid: options.uuid,
        };
        credentials
            .set_encrypted_session_with_recovery(
                &options.remote_name,
                &config,
                secret.clone(),
                budget.clone(),
                journals.clone(),
                multipart_journals.clone(),
            )
            .map_err(|_| {
                error(
                    "invalid_session_binding",
                    "invalid encrypted remote binding",
                )
            })?;
        let client = EncryptedHttpClient::new(
            &options.url,
            options.remote_name.clone(),
            credentials.clone(),
            secret.clone(),
        )
        .map_err(http_error)?;
        Ok(Self {
            credentials,
            config,
            remote_name: options.remote_name,
            secret,
            client,
            budget,
            provider,
            journals,
            multipart_journals,
            config_journal,
            host_directories: [
                PathBuf::from(options.checkpoint_directory),
                PathBuf::from(options.recovery_directory),
                PathBuf::from(options.staging_directory),
            ],
        })
    }

    #[napi(getter)]
    pub fn state(&self) -> &'static str {
        match self.secret.state() {
            SecretSessionState::Disconnected => "disconnected",
            SecretSessionState::Locked => "locked",
            SecretSessionState::Unlocking => "unlocking",
            SecretSessionState::Unlocked => "unlocked",
            SecretSessionState::Locking => "locking",
        }
    }

    #[napi(getter)]
    pub fn paused(&self) -> bool {
        self.secret.is_paused()
    }

    /// Consumes/wipes the supplied JS Buffer synchronously, including rejection.
    /// No JS Buffer or password string is retained across the async boundary.
    #[napi]
    pub fn unlock(&self, env: Env, mut password: Buffer) -> Result<AsyncBlock<()>> {
        if password.is_empty() || password.len() > 1024 {
            password.fill(0);
            return Err(secret_error(EncryptionError::InvalidPassword));
        }
        let input = Zeroizing::new(password.to_vec());
        password.fill(0);
        let future = self
            .client
            .prepare_password_unlock(input)
            .map_err(http_error)?;
        AsyncBlockBuilder::new(async move { future.await.map_err(http_error) }).build(&env)
    }

    /// Prepare a new fixed config and publish it create-if-absent. Config
    /// publication does not mean the repository's initial branch is connected.
    #[napi]
    pub fn initialize_config(&self, env: Env, password: Buffer) -> Result<AsyncBlock<String>> {
        initial_config(self, env, password, true)
    }

    /// Requires Locked admission; password restores the exact native journal.
    #[napi]
    pub fn resume_initial_config(&self, env: Env, password: Buffer) -> Result<AsyncBlock<String>> {
        initial_config(self, env, password, false)
    }

    /// Both buffers are copied and wiped before validation or async scheduling.
    #[napi]
    pub fn change_password(
        &self,
        env: Env,
        mut current_password: Buffer,
        mut new_password: Buffer,
    ) -> Result<AsyncBlock<String>> {
        if current_password.is_empty()
            || current_password.len() > 1024
            || new_password.is_empty()
            || new_password.len() > 1024
        {
            current_password.fill(0);
            new_password.fill(0);
            return Err(secret_error(EncryptionError::InvalidPassword));
        }
        let current = Zeroizing::new(current_password.to_vec());
        let next = Zeroizing::new(new_password.to_vec());
        current_password.fill(0);
        new_password.fill(0);
        if current.is_empty()
            || current.len() > 1024
            || next.is_empty()
            || next.len() > 1024
            || std::str::from_utf8(&current).is_err()
            || std::str::from_utf8(&next).is_err()
        {
            return Err(secret_error(EncryptionError::InvalidPassword));
        }
        let admission = self.secret.begin_operation().map_err(secret_error)?;
        let journal = self.config_journal.clone();
        let client = self.client.clone();
        AsyncBlockBuilder::new(async move {
            client
                .ensure_no_pending_config_mutation(journal.clone())
                .await
                .map_err(http_error)?;
            let candidate = client
                .prepare_password_change(current, next)
                .await
                .map_err(http_error)?;
            admission.check_active().map_err(secret_error)?;
            let outcome = client
                .publish_config_mutation(&candidate, journal)
                .await
                .map_err(http_error)?;
            config_outcome(outcome)
        })
        .build(&env)
    }

    #[napi]
    pub fn resume_password_change(&self, env: Env) -> Result<AsyncBlock<String>> {
        let admission = self.secret.begin_operation().map_err(secret_error)?;
        let journal = self.config_journal.clone();
        let client = self.client.clone();
        AsyncBlockBuilder::new(async move {
            admission.check_active().map_err(secret_error)?;
            let candidate = client
                .restore_config_mutation(journal.clone())
                .await
                .map_err(http_error)?;
            admission.check_active().map_err(secret_error)?;
            let outcome = client
                .reconcile_config_mutation(&candidate, journal.clone())
                .await
                .map_err(http_error)?;
            let outcome = if matches!(
                outcome,
                graft_sdk::e2ee::ConfigPublicationOutcome::Unconfirmed
            ) {
                admission.check_active().map_err(secret_error)?;
                client
                    .publish_config_mutation(&candidate, journal)
                    .await
                    .map_err(http_error)?
            } else {
                outcome
            };
            config_outcome(outcome)
        })
        .build(&env)
    }
}

// Keep the platform gate outside the N-API impl so generated registrations do
// not reference callbacks that Rust removes on unsupported hosts.
#[cfg(unix)]
#[napi]
impl NodeEncryptedRemoteSession {
    /// List admitted local ciphertext recovery, without sending account data.
    #[napi]
    pub fn retained_uploads(&self) -> Result<String> {
        let operation = self.secret.begin_operation().map_err(secret_error)?;
        let provider = graft_sdk::e2ee::FileMultipartJournalProvider::open(
            self.host_directories[1].join("multipart"),
            self.secret.binding(),
        )
        .map_err(secret_error)?;
        let entries = provider
            .retained_uploads(self.secret.binding())
            .map_err(secret_error)?;
        operation.check_active().map_err(secret_error)?;
        let entries: Vec<_> = entries
            .into_iter()
            .map(|entry| {
                serde_json::json!({
                    "opaqueKey":entry.opaque_key,"ciphertextLength":entry.ciphertext_length,
                    "serverSessionRecorded":entry.server_session_recorded,
                })
            })
            .collect();
        serde_json::to_string(&entries).map_err(|_| secret_error(EncryptionError::InvalidEncoding))
    }

    #[napi]
    pub fn resume_retained_upload(
        &self,
        env: Env,
        opaque_key: String,
    ) -> Result<AsyncBlock<String>> {
        retained_upload(self, env, opaque_key, false)
    }

    #[napi]
    pub fn restart_retained_upload(
        &self,
        env: Env,
        opaque_key: String,
    ) -> Result<AsyncBlock<String>> {
        retained_upload(self, env, opaque_key, true)
    }
}

#[napi]
impl NodeEncryptedRemoteSession {
    #[napi]
    pub fn unlock_remembered(&self, env: Env) -> Result<AsyncBlock<()>> {
        let future = self
            .client
            .prepare_remembered_unlock(self.provider()?)
            .map_err(http_error)?;
        AsyncBlockBuilder::new(async move { future.await.map_err(http_error) }).build(&env)
    }

    #[napi]
    pub async fn remember(&self) -> Result<()> {
        self.secret
            .remember_key(self.provider()?)
            .await
            .map_err(secret_error)
    }

    #[napi]
    pub async fn lock(&self) {
        self.secret.lock().await;
    }

    #[napi]
    pub fn pause(&self, paused: bool) {
        self.secret.pause(paused);
    }

    #[napi]
    pub async fn disconnect(&self) -> Result<()> {
        self.credentials
            .clear_encrypted_session(&self.remote_name)
            .await
            .map_err(|_| error("disconnect_failed", "encrypted session disconnect failed"))?;
        self.credentials
            .clear_http_bearer_token(&self.remote_name)
            .map_err(|_| error("invalid_credential", "credential removal failed"))
    }

    /// Same immutable host identity only; account changes require a new handle.
    #[napi]
    pub fn reconnect(&self, bearer_token: String) -> Result<()> {
        if self.secret.state() != SecretSessionState::Disconnected {
            return Err(secret_error(EncryptionError::SessionBusy));
        }
        self.credentials
            .set_http_bearer_token(&self.remote_name, bearer_token)
            .map_err(|_| error("invalid_credential", "credential replacement failed"))?;
        self.credentials
            .set_encrypted_session_with_recovery(
                &self.remote_name,
                &self.config,
                self.secret.clone(),
                self.budget.clone(),
                self.journals.clone(),
                self.multipart_journals.clone(),
            )
            .map_err(|_| {
                error(
                    "invalid_session_binding",
                    "encrypted session registration failed",
                )
            })?;
        self.secret.reconnect().map_err(secret_error)
    }

    /// Host must verify the refreshed token has unchanged domain/subject.
    #[napi]
    pub fn refresh_bearer_token(
        &self,
        authentication_domain: String,
        account_subject: String,
        bearer_token: String,
    ) -> Result<()> {
        self.credentials
            .set_bound_http_bearer_token(
                &self.remote_name,
                &authentication_domain,
                &account_subject,
                bearer_token,
            )
            .map_err(|_| error("identity_changed", "credential identity mismatch"))
    }

    #[napi]
    pub async fn forget(&self) -> Result<()> {
        let result = self.secret.forget_key(self.provider()?).await;
        self.disconnect().await?;
        result.map_err(secret_error)
    }
}

impl NodeEncryptedRemoteSession {
    fn provider(&self) -> Result<Arc<dyn RememberedKeyProvider>> {
        self.provider
            .clone()
            .ok_or_else(|| secret_error(EncryptionError::SecureStorageUnavailable))
    }
}

fn parse_uuid(value: &str) -> Result<RepositoryId> {
    if value.len() != 36 || !value.is_ascii() {
        return Err(secret_error(EncryptionError::InvalidSessionBinding));
    }
    let mut bytes = [0_u8; 16];
    let mut hex = String::with_capacity(32);
    for (index, byte) in value.bytes().enumerate() {
        if [8, 13, 18, 23].contains(&index) {
            if byte != b'-' {
                return Err(secret_error(EncryptionError::InvalidSessionBinding));
            }
        } else if byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) {
            hex.push(char::from(byte));
        } else {
            return Err(secret_error(EncryptionError::InvalidSessionBinding));
        }
    }
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .map_err(|_| secret_error(EncryptionError::InvalidSessionBinding))?;
    }
    RepositoryId::from_bytes(bytes).map_err(secret_error)
}

fn recovery_provider(
    path: &str,
    binding: &SessionBinding,
    create: bool,
) -> Result<Arc<dyn graft_sdk::e2ee::RefMutationJournalProvider>> {
    #[cfg(unix)]
    {
        let provider = if create {
            graft_sdk::e2ee::FileRefMutationJournalProvider::create(path, binding)
        } else {
            graft_sdk::e2ee::FileRefMutationJournalProvider::open(path, binding)
        };
        Ok(Arc::new(provider.map_err(secret_error)?))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, binding, create);
        Err(error(
            "unsupported_encrypted_host",
            "approved recovery storage unavailable on this host",
        ))
    }
}

fn config_journal(
    path: &str,
    binding: &SessionBinding,
    create: bool,
) -> Result<Arc<dyn graft_sdk::e2ee::ConfigMutationJournal>> {
    #[cfg(unix)]
    {
        let journal = if create {
            graft_sdk::e2ee::FileConfigMutationJournal::create_in_namespace(path, binding)
        } else {
            graft_sdk::e2ee::FileConfigMutationJournal::open(path, binding)
        };
        Ok(Arc::new(journal.map_err(secret_error)?))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, binding, create);
        Err(secret_error(EncryptionError::SecureStorageUnavailable))
    }
}

fn multipart_provider(
    recovery: &str,
    binding: &SessionBinding,
    create: bool,
) -> Result<Arc<dyn graft_sdk::e2ee::MultipartJournalProvider>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let path = PathBuf::from(recovery).join("multipart");
        let provider = if create {
            // This path is allocated only after claiming a new checkpoint. An
            // existing enrollment must open the exact retained namespace.
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .map_err(|error| {
                    secret_error(EncryptionError::CheckpointUnavailable(error.kind()))
                })?;
            std::fs::File::open(recovery)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    secret_error(EncryptionError::CheckpointUnavailable(error.kind()))
                })?;
            graft_sdk::e2ee::FileMultipartJournalProvider::create(path, binding)
        } else {
            graft_sdk::e2ee::FileMultipartJournalProvider::open(path, binding)
        };
        Ok(Arc::new(provider.map_err(secret_error)?))
    }
    #[cfg(not(unix))]
    {
        let _ = (recovery, binding, create);
        Err(secret_error(EncryptionError::SecureStorageUnavailable))
    }
}

#[cfg(unix)]
fn retained_upload(
    authority: &NodeEncryptedRemoteSession,
    env: Env,
    key: String,
    restart: bool,
) -> Result<AsyncBlock<String>> {
    let admission = authority.secret.begin_operation().map_err(secret_error)?;
    let client = authority.client.clone();
    let provider = authority.multipart_journals.clone();
    let staging = authority.budget.private_directory.clone();
    AsyncBlockBuilder::new(async move {
        admission.check_active().map_err(secret_error)?;
        let verification = tempfile::Builder::new()
            .prefix("retained-upload-")
            .tempdir_in(staging)
            .map_err(|error| secret_error(EncryptionError::CheckpointUnavailable(error.kind())))?;
        let outcome = if restart {
            client
                .restart_retained_object_multipart(&key, provider, verification.path())
                .await
        } else {
            client
                .resume_retained_object_multipart(&key, provider, verification.path())
                .await
        }
        .map_err(http_error)?;
        admission.check_active().map_err(secret_error)?;
        use graft_sdk::e2ee::ObjectPublicationOutcome as Outcome;
        Ok(match outcome {
            Outcome::Created => "created",
            Outcome::Reused => "reused",
            Outcome::Unconfirmed => "unconfirmed",
            Outcome::OutcomeUnknown {
                error: Some(value @ graft_sdk::e2ee::EncryptedHttpError::HttpStatus(401 | 403)),
            } => return Err(http_error(value)),
            Outcome::OutcomeUnknown { .. } => "unknown",
        }
        .to_owned())
    })
    .build(&env)
}

fn initial_config(
    authority: &NodeEncryptedRemoteSession,
    env: Env,
    mut password: Buffer,
    create: bool,
) -> Result<AsyncBlock<String>> {
    if password.is_empty() || password.len() > 1024 {
        password.fill(0);
        return Err(secret_error(EncryptionError::InvalidPassword));
    }
    let input = Zeroizing::new(password.to_vec());
    password.fill(0);
    #[cfg(unix)]
    {
        let secret = authority.secret.clone();
        let future = secret
            .prepare_native_initial_config(input, authority.host_directories[1].clone(), create)
            .map_err(secret_error)?;
        let client = authority.client.clone();
        AsyncBlockBuilder::new(async move {
            use graft_sdk::e2ee::InitialConfigPublicationOutcome as Outcome;
            let journal = future.await.map_err(secret_error)?;
            let admission = secret.begin_operation().map_err(secret_error)?;
            admission.check_active().map_err(secret_error)?;
            let outcome = if create {
                client
                    .publish_initial_config(journal.clone())
                    .await
                    .map_err(http_error)?
            } else {
                client
                    .reconcile_initial_config(journal.clone())
                    .await
                    .map_err(http_error)?
            };
            let outcome = if !create && matches!(outcome, Outcome::Unconfirmed) {
                admission.check_active().map_err(secret_error)?;
                client
                    .publish_initial_config(journal)
                    .await
                    .map_err(http_error)?
            } else {
                outcome
            };
            drop(admission);
            Ok(match outcome {
                Outcome::Confirmed => "config_published",
                Outcome::Unconfirmed => "unconfirmed",
                Outcome::Conflict => {
                    secret.lock().await;
                    "conflict"
                }
                Outcome::OutcomeUnknown {
                    error: Some(value @ graft_sdk::e2ee::EncryptedHttpError::HttpStatus(401 | 403)),
                } => return Err(http_error(value)),
                Outcome::OutcomeUnknown { .. } => "unknown",
            }
            .to_owned())
        })
        .build(&env)
    }
    #[cfg(not(unix))]
    {
        let _ = (authority, env, create, input);
        Err(secret_error(EncryptionError::SecureStorageUnavailable))
    }
}

fn config_outcome(outcome: graft_sdk::e2ee::ConfigPublicationOutcome) -> Result<String> {
    use graft_sdk::e2ee::ConfigPublicationOutcome as Outcome;
    // Explicit result labels contain no config, keys, passwords or provider handles.
    Ok(match outcome {
        Outcome::Confirmed => "confirmed",
        Outcome::Unconfirmed => "unconfirmed",
        Outcome::Conflict => "conflict",
        Outcome::OutcomeUnknown {
            error: Some(value @ graft_sdk::e2ee::EncryptedHttpError::HttpStatus(401 | 403)),
        } => return Err(http_error(value)),
        Outcome::OutcomeUnknown { .. } => "unknown",
    }
    .into())
}

fn checkpoint_store(
    path: &str,
    binding: &SessionBinding,
    create: bool,
) -> Result<Arc<dyn graft_sdk::e2ee::CheckpointStore>> {
    #[cfg(unix)]
    {
        use graft_sdk::e2ee::FileCheckpointStore;
        let store = if create {
            FileCheckpointStore::create(path, binding)
        } else {
            FileCheckpointStore::open(path, binding)
        }
        .map_err(secret_error)?;
        Ok(Arc::new(store))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, binding, create);
        Err(error(
            "unsupported_encrypted_host",
            "approved checkpoint storage unavailable on this host",
        ))
    }
}

fn validate_private_directory(path: &std::path::Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        error(
            "invalid_session_binding",
            "private staging directory unavailable",
        )
    })?;
    if !path.is_absolute() || !metadata.is_dir() {
        return Err(secret_error(EncryptionError::InvalidSessionBinding));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(secret_error(EncryptionError::InvalidSessionBinding));
        }
    }
    Ok(())
}

fn key_provider(
    application: Option<String>,
    group: Option<String>,
) -> Result<Option<Arc<dyn RememberedKeyProvider>>> {
    match (application, group) {
        (None, None) => Ok(None),
        (Some(application), Some(group)) => {
            #[cfg(all(target_os = "macos", feature = "macos-keychain"))]
            {
                Ok(Some(Arc::new(
                    graft_sdk::e2ee::MacKeychainProvider::new(application, group)
                        .map_err(secret_error)?,
                )))
            }
            #[cfg(not(all(target_os = "macos", feature = "macos-keychain")))]
            {
                let _ = (application, group);
                Err(secret_error(EncryptionError::SecureStorageUnavailable))
            }
        }
        _ => Err(secret_error(EncryptionError::InvalidSessionBinding)),
    }
}

fn error(code: &str, message: &str) -> Error {
    let label = if code == "cancelled" {
        "GRAFT_SDK_CANCELLED".to_owned()
    } else {
        format!("GRAFT_SDK_E2EE_{}", code.to_ascii_uppercase())
    };
    Error::new(Status::GenericFailure, format!("[{label}] {message}"))
}
pub(super) fn secret_error(value: EncryptionError) -> Error {
    let code = graft_sdk::encryption_error_code(&value);
    Error::new(
        Status::GenericFailure,
        format!("[{}] {value}", code.as_str()),
    )
}
fn http_error(value: graft_sdk::e2ee::EncryptedHttpError) -> Error {
    if let graft_sdk::e2ee::EncryptedHttpError::Encryption(value) = value {
        return secret_error(value);
    }
    let code = graft_sdk::encrypted_http_error_code(&value);
    Error::new(
        Status::GenericFailure,
        format!("[{}] encrypted remote operation failed", code.as_str()),
    )
}
