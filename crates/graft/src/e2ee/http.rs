use super::bundle;
use super::download::{CiphertextBody, finish_staging, private_staging_file, stream_plaintext};
use super::session::SessionEpoch;
use super::{
    EncryptedBundleEntry, EncryptedConfig, EncryptionError, ObjectPublicationOutcome, ObjectReader,
    PreparedEncryptedObject, PreparedRefMutation, RefObservation, RefPublicationOutcome, RefTarget,
    RememberedKeyProvider, SecretSession, SessionOperation, mutation, object, reference,
};
use crate::remote::RemoteCredentials;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fmt, ops::Range, path::Path, sync::Arc, time::Duration};

const MAX_RANGE_PLAINTEXT: u64 = 64 * 1024 * 1024;
// Includes upload, headers and the complete streamed response, even when a peer
// keeps sending fragments more frequently than the per-read timeout.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[path = "http/multipart.rs"]
mod multipart;

#[path = "aggregate_fetch.rs"]
mod aggregate_fetch;
#[path = "aggregate_receive.rs"]
mod aggregate_receive;
pub(crate) use aggregate_receive::{PreparedReceiveBundle, ReceiveBundleOutcome};

#[derive(Debug, thiserror::Error)]
pub enum EncryptedHttpError {
    #[error(transparent)]
    Encryption(#[from] EncryptionError),
    #[error("invalid encrypted HTTP endpoint")]
    InvalidEndpoint,
    #[error("encrypted HTTP transport failed")]
    Transport(#[source] reqwest::Error),
    #[error("encrypted HTTP server returned status {0}")]
    HttpStatus(u16),
    #[error("encrypted HTTP protocol mismatch")]
    ProtocolMismatch,
    #[error("encrypted HTTP descriptor is invalid or unsupported")]
    UnsupportedProfile,
    #[error("encrypted repository configuration is missing")]
    ConfigMissing,
    #[error("encrypted download staging failed ({0:?})")]
    LocalIo(std::io::ErrorKind),
}

type Result<T> = std::result::Result<T, EncryptedHttpError>;

/// Authenticated local metadata. No secret key material or plaintext Debug output.
pub struct EncryptedObjectMetadata {
    pub logical_key: String,
    pub plaintext_length: u64,
    pub plaintext_sha256: [u8; 32],
}

/// A private temporary plaintext file with authenticated envelope and full SHA-256.
/// Repository callers must still validate the authoritative Graft object identity.
pub struct DownloadedEncryptedObject {
    pub(super) file: std::fs::File,
    pub(super) metadata: EncryptedObjectMetadata,
}

impl DownloadedEncryptedObject {
    pub fn metadata(&self) -> &EncryptedObjectMetadata {
        &self.metadata
    }

    /// The file cursor starts at zero. Dropping the final handle releases temporary storage.
    pub fn into_file(self) -> std::fs::File {
        self.file
    }
}

/// Native transport for the pinned encrypted repository profile.
#[derive(Clone)]
pub struct EncryptedHttpClient {
    base: reqwest::Url,
    remote_name: String,
    credentials: RemoteCredentials,
    session: SecretSession,
    client: reqwest::Client,
    transfer_timeout: Duration,
    verified: Arc<tokio::sync::Mutex<Option<VerifiedProfile>>>,
}

#[derive(Clone, Copy)]
struct VerifiedProfile {
    epoch: SessionEpoch,
    limits: Limits,
}

impl fmt::Debug for EncryptedHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedHttpClient")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl EncryptedHttpClient {
    /// Local preflight for hosts beginning a new password-change transaction.
    /// Binding still compares under the journal lock to resolve concurrent callers.
    pub async fn ensure_no_pending_config_mutation(
        &self,
        journal: Arc<dyn super::ConfigMutationJournal>,
    ) -> Result<()> {
        let operation = self.session.begin_operation()?;
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            if journal.load(session.binding())?.is_some() {
                return Err(EncryptionError::StaleObservation);
            }
            operation.check_active()
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))?
        .map_err(Into::into)
    }

    pub async fn restore_config_mutation(
        &self,
        journal: Arc<dyn super::ConfigMutationJournal>,
    ) -> Result<super::PreparedConfigMutation> {
        let operation = self.operation().await?;
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            let record = journal.load_recoverable(session.binding())?.ok_or(
                EncryptionError::CheckpointUnavailable(std::io::ErrorKind::NotFound),
            )?;
            operation.restore_config_mutation(record)
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))?
        .map_err(Into::into)
    }

    pub async fn publish_config_mutation(
        &self,
        candidate: &super::PreparedConfigMutation,
        journal: Arc<dyn super::ConfigMutationJournal>,
    ) -> Result<super::ConfigPublicationOutcome> {
        self.config_mutation_publication(candidate, journal, true)
            .await
    }

    /// Inspect the retained candidate without remote mutation. Confirmed reads
    /// may durably advance the checkpoint and retire the exact local journal.
    pub async fn reconcile_config_mutation(
        &self,
        candidate: &super::PreparedConfigMutation,
        journal: Arc<dyn super::ConfigMutationJournal>,
    ) -> Result<super::ConfigPublicationOutcome> {
        self.config_mutation_publication(candidate, journal, false)
            .await
    }

    async fn config_mutation_publication(
        &self,
        candidate: &super::PreparedConfigMutation,
        journal: Arc<dyn super::ConfigMutationJournal>,
        publish: bool,
    ) -> Result<super::ConfigPublicationOutcome> {
        use super::ConfigPublicationOutcome as Outcome;
        let admission = self.session.begin_operation()?;
        candidate.record(&admission)?;
        drop(admission);
        let operation = self.operation().await?;
        let record = candidate.record(&operation)?;
        operation.restore_config_mutation(record.clone())?;
        self.ensure_request_budget(&operation, 206).await?;
        let session = self.session.clone();
        let retained = record.clone();
        let persist = journal.clone();
        let operation = tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            if publish {
                persist.bind(session.binding(), &retained)?;
            } else if persist.load_recoverable(session.binding())? != Some(retained) {
                return Err(EncryptionError::StaleObservation);
            }
            operation.check_active()?;
            Ok::<_, EncryptionError>(operation)
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))??;
        let mut mismatch = false;
        let mut cause = None;
        if publish {
            let url = self.object_url("cas", "e2ee/config")?;
            let expected: String = record
                .expected_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let request = self.credentials.authorize_encrypted_request(
                &self.remote_name,
                self.client
                    .post(url)
                    .header("Graft-Protocol", "1")
                    .header("Content-Type", "application/octet-stream")
                    .header("x-graft-expected-present", "true")
                    .header("x-graft-expected-hex", expected)
                    .header("Content-Length", 206)
                    .body(record.replacement_bytes().to_vec()),
            );
            operation.check_active()?;
            let response = tokio::select! {
                response = request.send() => response.map_err(transport_error),
                _ = operation.cancelled() => return Ok(Outcome::OutcomeUnknown { error: Some(EncryptionError::Cancelled.into()) }),
            };
            match response {
                Ok(response) => {
                    if let Err(error) = validate_protocol(&response) {
                        return Ok(Outcome::OutcomeUnknown { error: Some(error) });
                    }
                    mismatch = matches!(
                        response.status(),
                        reqwest::StatusCode::CONFLICT | reqwest::StatusCode::PRECONDITION_FAILED
                    );
                    if response.status() != reqwest::StatusCode::NO_CONTENT {
                        cause = Some(EncryptedHttpError::HttpStatus(response.status().as_u16()));
                    } else if response.content_length().is_some_and(|length| length != 0) {
                        return Ok(Outcome::OutcomeUnknown {
                            error: Some(EncryptedHttpError::ProtocolMismatch),
                        });
                    }
                }
                Err(error) => cause = Some(error),
            }
        }
        let bytes = match self.get(&operation, Some("e2ee/config"), 206, true).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return Ok(Outcome::OutcomeUnknown {
                    error: Some(EncryptedHttpError::ConfigMissing),
                });
            }
            Err(error) => return Ok(Outcome::OutcomeUnknown { error: Some(error) }),
        };
        let observed = match EncryptedConfig::parse(&bytes).and_then(|config| {
            config.authenticate(operation.root()?)?;
            Ok(config)
        }) {
            Ok(config) => config,
            Err(error) => return Ok(Outcome::OutcomeUnknown { error: Some(error.into()) }),
        };
        let checkpoint_guard = (|| {
            let current = observed.checkpoint(operation.root()?)?;
            EncryptedConfig::parse(record.expected_bytes())?
                .checkpoint(operation.root()?)?
                .require_successor(&current)?;
            if let Some(previous) = self.session.checkpoint() {
                previous.require_successor(&current)?;
            }
            Ok::<_, EncryptionError>(())
        })();
        if let Err(error) = checkpoint_guard {
            return Ok(Outcome::OutcomeUnknown { error: Some(error.into()) });
        }
        let confirmed = observed.as_bytes() == record.replacement_bytes();
        if !confirmed && observed.as_bytes() == record.expected_bytes() {
            return Ok(Outcome::Unconfirmed);
        }
        if !confirmed && !mismatch {
            return Ok(Outcome::OutcomeUnknown { error: cause });
        }
        let operation = if confirmed {
            match operation.observe_config_durable(observed.clone()).await {
                Ok(operation) => operation,
                Err(error) => return Ok(Outcome::OutcomeUnknown { error: Some(error.into()) }),
            }
        } else {
            operation
        };
        let retired = tokio::task::spawn_blocking(move || {
            operation.retire_config_mutation(&observed, journal.as_ref(), &record)?;
            operation.check_active()
        })
        .await;
        match retired {
            Ok(Ok(())) if confirmed => Ok(Outcome::Confirmed),
            Ok(Ok(())) => Ok(Outcome::Conflict),
            Ok(Err(error)) => Ok(Outcome::OutcomeUnknown { error: Some(error.into()) }),
            Err(_) => Ok(Outcome::OutcomeUnknown {
                error: Some(
                    EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other).into(),
                ),
            }),
        }
    }

    /// Prepare an authorized fixed config candidate from a fresh remote read.
    /// This is not password-change completion: publication still requires a
    /// trusted durable journal, exact CAS, reconciliation and checkpoint confirmation.
    pub async fn prepare_password_change(
        &self,
        current_password: zeroize::Zeroizing<Vec<u8>>,
        new_password: zeroize::Zeroizing<Vec<u8>>,
    ) -> Result<super::PreparedConfigMutation> {
        super::config::validate_password(&current_password)?;
        super::config::validate_password(&new_password)?;
        let operation = self.operation().await?;
        let bytes = self
            .get(&operation, Some("e2ee/config"), 206, true)
            .await?
            .ok_or(EncryptedHttpError::ConfigMissing)?;
        let current = EncryptedConfig::parse(&bytes)?;
        tokio::task::spawn_blocking(move || {
            operation.prepare_password_change(&current, current_password, new_password)
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))?
        .map_err(Into::into)
    }

    /// Prepare a forgotten-password reset with fresh trusted native OS authorization.
    /// Requires an already unlocked root; it neither unlocks nor publishes config.
    /// Publication uses the same durable exact-CAS recovery contract as password change.
    pub async fn prepare_password_reset(
        &self,
        new_password: zeroize::Zeroizing<Vec<u8>>,
        authorizer: Arc<dyn super::PasswordResetAuthorizer>,
    ) -> Result<super::PreparedConfigMutation> {
        super::config::validate_password(&new_password)?;
        let operation = self.operation().await?;
        let bytes = self
            .get(&operation, Some("e2ee/config"), 206, true)
            .await?
            .ok_or(EncryptedHttpError::ConfigMissing)?;
        let current = EncryptedConfig::parse(&bytes)?;
        tokio::task::spawn_blocking(move || {
            operation.prepare_password_reset(&current, new_password, authorizer.as_ref())
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))?
        .map_err(Into::into)
    }

    /// Publish the exact generation-1 config from an explicitly created journal.
    /// The host must first unlock this session from that local candidate. This
    /// bypasses only the remote-config bootstrap check, never root authentication,
    /// account authorization, descriptor pinning, pause or lock admission.
    /// Confirmed means config-published, not that the initial branch is connected.
    pub async fn publish_initial_config(
        &self,
        journal: Arc<dyn super::InitializationJournal>,
    ) -> Result<super::InitialConfigPublicationOutcome> {
        self.initial_config_publication(journal, true).await
    }

    /// Inspect a retained initialization candidate without sending a mutation.
    pub async fn reconcile_initial_config(
        &self,
        journal: Arc<dyn super::InitializationJournal>,
    ) -> Result<super::InitialConfigPublicationOutcome> {
        self.initial_config_publication(journal, false).await
    }

    async fn initial_config_publication(
        &self,
        journal: Arc<dyn super::InitializationJournal>,
        publish: bool,
    ) -> Result<super::InitialConfigPublicationOutcome> {
        use super::{InitialConfigPublicationOutcome as Outcome, InitializationStage as Stage};
        let operation = self.session.begin_operation()?;
        let session = self.session.clone();
        let load = journal.clone();
        // The operation remains owned by persistence even if its caller exits.
        let (operation, record) = tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            let record = load.load(session.binding())?;
            operation.check_active()?;
            Ok::<_, EncryptionError>((operation, record))
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))??;
        if !matches!(record.stage, Stage::Prepared | Stage::ConfigPublished) {
            return Err(EncryptionError::StaleObservation.into());
        }
        if record.config.generation() != 1 {
            return Err(EncryptionError::InvalidEncoding.into());
        }
        record.config.authenticate(operation.root()?)?;
        // A retained checkpoint may have advanced since this initialization
        // journal was prepared. Reject that rollback before any config upload.
        let operation = operation
            .observe_config_durable(record.config.clone())
            .await?;
        let repository = operation.root()?.repository();
        let epoch = operation.epoch()?;
        let mut verified = tokio::select! {
            guard = self.verified.lock() => guard,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        let descriptor = self
            .get(&operation, None, 256 * 1024, false)
            .await?
            .ok_or(EncryptedHttpError::UnsupportedProfile)?;
        let limits = validate_descriptor(&descriptor, repository)?;
        if limits
            .max_request_bytes
            .is_some_and(|maximum| maximum < 206)
        {
            return Err(EncryptionError::TooLarge.into());
        }
        let mut mismatch = false;
        let mut cause = None;
        if publish && record.stage == Stage::Prepared {
            let request = self.credentials.authorize_encrypted_request(
                &self.remote_name,
                self.client
                    .put(self.object_url("raw-if-not-exists", "e2ee/config")?)
                    .header("Graft-Protocol", "1")
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Length", 206)
                    .body(record.config.as_bytes().to_vec()),
            );
            operation.check_active()?;
            let response = tokio::select! {
                response = request.send() => response.map_err(transport_error),
                _ = operation.cancelled() => return Ok(Outcome::OutcomeUnknown {
                    error: Some(EncryptionError::Cancelled.into()),
                }),
            };
            match response {
                Ok(response) => {
                    if let Err(error) = validate_protocol(&response) {
                        return Ok(Outcome::OutcomeUnknown { error: Some(error) });
                    }
                    let status = response.status();
                    if status == reqwest::StatusCode::NO_CONTENT {
                        if response.content_length().is_some_and(|length| length != 0) {
                            return Ok(Outcome::OutcomeUnknown {
                                error: Some(EncryptedHttpError::ProtocolMismatch),
                            });
                        }
                    } else {
                        mismatch = status == reqwest::StatusCode::PRECONDITION_FAILED;
                        cause = Some(EncryptedHttpError::HttpStatus(status.as_u16()));
                    }
                }
                Err(error) => cause = Some(error),
            }
        }
        // Even a success response requires reading back the exact authenticated
        // config. A response loss cannot justify generating a different candidate.
        let observed = match self.get(&operation, Some("e2ee/config"), 206, true).await {
            Ok(bytes) => bytes,
            Err(error) => return Ok(Outcome::OutcomeUnknown { error: Some(error) }),
        };
        let Some(bytes) = observed else {
            return Ok(if record.stage == Stage::ConfigPublished {
                Outcome::OutcomeUnknown {
                    error: Some(EncryptedHttpError::ConfigMissing),
                }
            } else {
                Outcome::Unconfirmed
            });
        };
        let config = match EncryptedConfig::parse(&bytes) {
            Ok(config) if config.repository() == repository => config,
            Ok(_) => {
                return Ok(Outcome::OutcomeUnknown {
                    error: Some(EncryptionError::IdentityChanged.into()),
                });
            }
            Err(error) => return Ok(Outcome::OutcomeUnknown { error: Some(error.into()) }),
        };
        if config.as_bytes() != record.config.as_bytes() {
            return Ok(if mismatch {
                Outcome::Conflict
            } else {
                Outcome::OutcomeUnknown { error: cause }
            });
        }
        let operation = match operation.observe_config_durable(config).await {
            Ok(operation) => operation,
            Err(error) => return Ok(Outcome::OutcomeUnknown { error: Some(error.into()) }),
        };
        let session = self.session.clone();
        let persisted = tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            // Includes reconfirming fsync after a lost persistence acknowledgement.
            journal.advance(session.binding(), Stage::Prepared, Stage::ConfigPublished)?;
            operation.check_active()?;
            Ok::<_, EncryptionError>(operation)
        })
        .await;
        let operation = match persisted {
            Ok(Ok(operation)) => operation,
            Ok(Err(error)) => return Ok(Outcome::OutcomeUnknown { error: Some(error.into()) }),
            Err(_) => {
                return Ok(Outcome::OutcomeUnknown {
                    error: Some(
                        EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other).into(),
                    ),
                });
            }
        };
        if let Err(error) = operation.check_active() {
            return Ok(Outcome::OutcomeUnknown { error: Some(error.into()) });
        }
        *verified = Some(VerifiedProfile { epoch, limits });
        Ok(Outcome::Confirmed)
    }

    /// Bounded opaque discovery; the service cannot prove listing completeness.
    pub async fn list_opaque(
        &self,
        class: super::EncryptedListClass,
        budget: super::DiscoveryBudget,
    ) -> Result<Vec<String>> {
        let mut listing = super::discovery::Listing::new(class, budget)?;
        let operation = self.operation().await?;
        loop {
            let (prefix, cursor, limit) = listing.query()?;
            let mut url = self.object_url("list", "")?;
            url.query_pairs_mut()
                .append_pair("prefix", prefix)
                .append_pair("limit", &limit.to_string());
            if let Some(cursor) = cursor {
                url.query_pairs_mut().append_pair("cursor", cursor);
            }
            let request = self.credentials.authorize_encrypted_request(
                &self.remote_name,
                self.client
                    .get(url)
                    .header("Graft-Protocol", "1")
                    .timeout(Duration::from_secs(60).min(self.transfer_timeout)),
            );
            operation.check_active()?;
            let response = tokio::select! {
                response = request.send() => response.map_err(transport_error)?,
                _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
            };
            let bytes = read_response(&operation, response, 1024 * 1024, false)
                .await?
                .ok_or(EncryptedHttpError::ProtocolMismatch)?;
            if listing.accept(&bytes)? {
                operation.check_active()?;
                return Ok(listing.finish());
            }
        }
    }

    pub async fn discover_ref(&self, opaque_key: &str) -> Result<Option<(String, RefObservation)>> {
        reference::validate_remote_key(opaque_key)?;
        let operation = self.operation().await?;
        let Some(bytes) = self
            .get(&operation, Some(opaque_key), 16 * 1024, true)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(operation.discover_ref(opaque_key, &bytes)?))
    }
    /// Explicit first-device unlock. Only public encrypted config is fetched before KDF.
    pub async fn unlock(&self, password: zeroize::Zeroizing<Vec<u8>>) -> Result<()> {
        self.prepare_password_unlock(password)?.await
    }

    /// Registers the request synchronously before a host queues asynchronous work.
    /// Dropping the returned future releases its ticket and wipes the password.
    pub fn prepare_password_unlock(
        &self,
        password: zeroize::Zeroizing<Vec<u8>>,
    ) -> Result<impl std::future::Future<Output = Result<()>> + Send + 'static> {
        let input = self.session.begin_password_unlock(password)?;
        let client = self.clone();
        Ok(async move { client.complete_password_unlock(input).await })
    }

    async fn complete_password_unlock(&self, input: super::session::PasswordUnlock) -> Result<()> {
        let operation = input.operation();
        let mut verified = tokio::select! {
            guard = self.verified.lock() => guard,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        let (config, limits) = self.unlock_config(operation).await?;
        self.session.unlock_request(config, input).await?;
        let operation = match self.session.begin_operation() {
            Ok(operation) => operation,
            // Unlocking is explicit maintenance, independent of paused sync admission.
            // Revalidate online after resume rather than admitting a paused data task.
            Err(EncryptionError::Paused) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        *verified = Some(VerifiedProfile { epoch: operation.epoch()?, limits });
        Ok(())
    }

    /// Explicit host-authorized online unlock using approved remembered storage.
    /// Fetches only the public descriptor/config before loading a root. Never
    /// invoked automatically by data operations, lock, disconnect or reconnect.
    pub async fn unlock_remembered(&self, provider: Arc<dyn RememberedKeyProvider>) -> Result<()> {
        self.prepare_remembered_unlock(provider)?.await
    }

    /// Registers an explicit remembered-key request before host async scheduling.
    pub fn prepare_remembered_unlock(
        &self,
        provider: Arc<dyn RememberedKeyProvider>,
    ) -> Result<impl std::future::Future<Output = Result<()>> + Send + 'static> {
        let input = self.session.begin_remembered_unlock(provider)?;
        let client = self.clone();
        Ok(async move { client.complete_remembered_unlock(input).await })
    }

    async fn complete_remembered_unlock(
        &self,
        input: super::session::RememberedUnlock,
    ) -> Result<()> {
        let operation = input.operation();
        let mut verified = tokio::select! {
            guard = self.verified.lock() => guard,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        let (config, limits) = self.unlock_config(operation).await?;
        self.session
            .unlock_remembered_request(config, input)
            .await?;
        let operation = match self.session.begin_operation() {
            Ok(operation) => operation,
            Err(EncryptionError::Paused) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        *verified = Some(VerifiedProfile { epoch: operation.epoch()?, limits });
        Ok(())
    }

    async fn unlock_config(
        &self,
        operation: &SessionOperation,
    ) -> Result<(EncryptedConfig, Limits)> {
        let descriptor = self
            .get(operation, None, 256 * 1024, false)
            .await?
            .ok_or(EncryptedHttpError::UnsupportedProfile)?;
        let limits = validate_descriptor(&descriptor, self.session.binding().repository())?;
        let bytes = self
            .get(operation, Some("e2ee/config"), 206, true)
            .await?
            .ok_or(EncryptedHttpError::ConfigMissing)?;
        let config = EncryptedConfig::parse(&bytes)?;
        if config.repository() != self.session.binding().repository() {
            return Err(EncryptionError::IdentityChanged.into());
        }
        Ok((config, limits))
    }

    pub fn new(
        url: &str,
        remote_name: String,
        credentials: RemoteCredentials,
        session: SecretSession,
    ) -> Result<Self> {
        let base = validate_endpoint(url)?;
        if remote_name.is_empty()
            || remote_name
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
            || remote_name.contains(['/', '\\'])
        {
            return Err(EncryptedHttpError::InvalidEndpoint);
        }
        let client = transfer_client(TRANSFER_TIMEOUT)?;
        Ok(Self {
            base,
            remote_name,
            credentials,
            session,
            client,
            transfer_timeout: TRANSFER_TIMEOUT,
            verified: Arc::default(),
        })
    }

    /// Host-selected wall-clock limit for each HTTP exchange, including streamed
    /// upload and download bodies. Retry and multipart requests each get a fresh
    /// limit; mutation timeouts retain the exact journal candidate for recovery.
    pub fn with_transfer_timeout(mut self, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() {
            return Err(EncryptionError::TooLarge.into());
        }
        self.client = transfer_client(timeout)?;
        self.transfer_timeout = timeout;
        Ok(self)
    }

    /// First online use in each unlock epoch authenticates the current config.
    pub async fn verify(&self) -> Result<()> {
        let operation = self.operation().await?;
        operation.check_active()?;
        Ok(())
    }

    /// Returns authenticated ciphertext or transport-observed absence (not a freshness proof).
    pub async fn read_ref(&self, logical: &str) -> Result<RefObservation> {
        let operation = self.operation().await?;
        let key = reference::remote_key(operation.root()?, logical)?;
        let bytes = self.get(&operation, Some(&key), 16 * 1024, true).await?;
        Ok(operation.observe_ref(logical, bytes.as_deref())?)
    }

    /// The repository must validate fast-forward policy and publish dependencies first.
    pub async fn prepare_ref_update(
        &self,
        logical: &str,
        target: &RefTarget,
        expected: RefObservation,
    ) -> Result<PreparedRefMutation> {
        let operation = self.operation().await?;
        Ok(PreparedRefMutation::update(
            &operation, logical, target, expected,
        )?)
    }

    pub async fn prepare_ref_delete(
        &self,
        logical: &str,
        expected: RefObservation,
    ) -> Result<PreparedRefMutation> {
        let operation = self.operation().await?;
        Ok(PreparedRefMutation::delete(&operation, logical, expected)?)
    }

    /// Retry this same candidate; never prepare a new ciphertext for an uncertain request.
    /// Persist the exact candidate before any mutation dispatch. A cancelled
    /// caller cannot detach key-owning journal work from the lock/drain boundary.
    pub async fn publish_ref_durable(
        &self,
        candidate: &PreparedRefMutation,
        journal: Arc<dyn super::RefMutationJournal>,
    ) -> Result<RefPublicationOutcome> {
        let operation = self.session.begin_operation()?;
        let record = candidate.record(&operation)?;
        let session = self.session.clone();
        let operation = tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            journal.bind(session.binding(), &record)?;
            operation.check_active()?;
            Ok::<_, EncryptionError>(operation)
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))??;
        operation.check_active()?;
        drop(operation);
        self.publish_ref(candidate).await
    }

    /// Explicitly restore retained ciphertext into the current admitted epoch.
    /// This authenticates old/candidate bytes; it does not assert remote freshness.
    pub async fn restore_ref_mutation(
        &self,
        logical: &str,
        journal: Arc<dyn super::RefMutationJournal>,
    ) -> Result<PreparedRefMutation> {
        self.load_ref_mutation(logical, journal)
            .await?
            .ok_or(EncryptionError::CheckpointUnavailable(std::io::ErrorKind::NotFound).into())
    }

    pub(crate) async fn load_ref_mutation(
        &self,
        logical: &str,
        journal: Arc<dyn super::RefMutationJournal>,
    ) -> Result<Option<PreparedRefMutation>> {
        let operation = self.operation().await?;
        let session = self.session.clone();
        let logical = logical.to_owned();
        Ok(tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            journal
                .load(session.binding())?
                .map(|record| PreparedRefMutation::restore(&operation, &logical, record))
                .transpose()
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))??)
    }

    pub(crate) async fn retire_ref_mutation(
        &self,
        candidate: &PreparedRefMutation,
        journal: Arc<dyn super::RefMutationJournal>,
    ) -> Result<()> {
        let operation = self.session.begin_operation()?;
        let record = candidate.record(&operation)?;
        let session = self.session.clone();
        tokio::task::spawn_blocking(move || {
            operation.check_active()?;
            journal.retire(session.binding(), &record)?;
            operation.check_active()
        })
        .await
        .map_err(|_| EncryptionError::CheckpointUnavailable(std::io::ErrorKind::Other))??;
        Ok(())
    }

    /// Retry this same candidate; never prepare a new ciphertext for an uncertain request.
    /// Admission errors happen before dispatch. After dispatch, unknown outcomes retain causes.
    async fn publish_ref(&self, candidate: &PreparedRefMutation) -> Result<RefPublicationOutcome> {
        let admission = self.session.begin_operation()?;
        candidate.validate(&admission)?;
        drop(admission);
        let operation = self.operation().await?;
        candidate.validate(&operation)?;
        self.ensure_request_budget(
            &operation,
            candidate.replacement_bytes().map_or(0, <[u8]>::len) as u64,
        )
        .await?;
        let request = self.ref_mutation_request(candidate)?;
        operation.check_active()?;
        let response = tokio::select! {
            response = request.send() => response.map_err(transport_error),
            _ = operation.cancelled() => return Ok(EncryptionError::Cancelled.into()),
        };
        let (mismatch, cause) = match response {
            Ok(response) => {
                if let Err(error) = validate_protocol(&response) {
                    return Ok(RefPublicationOutcome::OutcomeUnknown { error: Some(error) });
                }
                let status = response.status();
                if status == reqwest::StatusCode::NO_CONTENT {
                    if response.content_length().is_some_and(|n| n != 0) {
                        return Ok(RefPublicationOutcome::OutcomeUnknown {
                            error: Some(EncryptedHttpError::ProtocolMismatch),
                        });
                    }
                    return Ok(RefPublicationOutcome::Confirmed);
                }
                (
                    matches!(
                        status,
                        reqwest::StatusCode::CONFLICT | reqwest::StatusCode::PRECONDITION_FAILED
                    ),
                    Some(EncryptedHttpError::HttpStatus(status.as_u16())),
                )
            }
            Err(error) => (false, Some(error)),
        };
        Ok(self
            .reconcile_ref_with(&operation, candidate, mismatch, cause)
            .await)
    }

    /// Explicitly inspect an uncertain candidate without sending another mutation.
    /// A different ref alone cannot prove that the original request failed.
    pub async fn reconcile_ref(
        &self,
        candidate: &PreparedRefMutation,
    ) -> Result<RefPublicationOutcome> {
        let operation = self.operation().await?;
        candidate.validate(&operation)?;
        Ok(self
            .reconcile_ref_with(&operation, candidate, false, None)
            .await)
    }

    async fn reconcile_ref_with(
        &self,
        operation: &SessionOperation,
        candidate: &PreparedRefMutation,
        mismatch: bool,
        cause: Option<EncryptedHttpError>,
    ) -> RefPublicationOutcome {
        let observed = match self
            .get(operation, Some(candidate.remote_key()), 16 * 1024, true)
            .await
        {
            Ok(bytes) => bytes,
            Err(error) => return RefPublicationOutcome::OutcomeUnknown { error: Some(error) },
        };
        if let Err(error) = operation.observe_ref(&candidate.logical, observed.as_deref()) {
            return error.into();
        }
        match mutation::classify(observed.as_deref(), candidate, mismatch) {
            RefPublicationOutcome::OutcomeUnknown { .. } => {
                RefPublicationOutcome::OutcomeUnknown { error: cause }
            }
            outcome => outcome,
        }
    }

    fn ref_mutation_request(
        &self,
        candidate: &PreparedRefMutation,
    ) -> Result<reqwest::RequestBuilder> {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| EncryptedHttpError::InvalidEndpoint)?;
        path.pop_if_empty()
            .push(if candidate.is_delete() { "cad" } else { "cas" });
        for segment in candidate.remote_key().split('/') {
            path.push(segment);
        }
        drop(path);
        let expected = candidate.expected_bytes();
        let hex: String = expected
            .unwrap_or(&[])
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let body = candidate.replacement_bytes().unwrap_or(&[]).to_vec();
        Ok(self.credentials.authorize_encrypted_request(
            &self.remote_name,
            self.client
                .post(url)
                .header("Graft-Protocol", "1")
                .header("Content-Type", "application/octet-stream")
                .header(
                    "x-graft-expected-present",
                    if expected.is_some() { "true" } else { "false" },
                )
                .header("x-graft-expected-hex", hex)
                .header("Content-Length", body.len())
                .body(body),
        ))
    }

    async fn ensure_request_budget(&self, operation: &SessionOperation, length: u64) -> Result<()> {
        let verified = tokio::select! {
            profile = self.verified.lock() => profile,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        if verified
            .as_ref()
            .and_then(|profile| profile.limits.max_request_bytes)
            .is_some_and(|maximum| length > maximum)
        {
            return Err(EncryptionError::TooLarge.into());
        }
        operation.check_active()?;
        Ok(())
    }

    /// Publish a previously validated fixed spool. Repository identity validation
    /// and dependency ordering are required before publishing a referencing ref.
    pub async fn publish_object(
        &self,
        candidate: &PreparedEncryptedObject,
    ) -> Result<ObjectPublicationOutcome> {
        let admission = self.session.begin_operation()?;
        candidate.validate(&admission)?;
        drop(admission);
        let operation = self.operation().await?;
        candidate.validate(&operation)?;
        self.ensure_request_budget(&operation, candidate.ciphertext_length())
            .await?;
        let (body, _cancel_body) = candidate.body();
        let url = self.object_url("raw-if-not-exists", candidate.remote_key())?;
        let request = self.credentials.authorize_encrypted_request(
            &self.remote_name,
            self.client
                .put(url)
                .header("Graft-Protocol", "1")
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", candidate.ciphertext_length())
                .body(body),
        );
        operation.check_active()?;
        let response = tokio::select! {
            response = request.send() => response.map_err(transport_error),
            _ = operation.cancelled() => return Ok(ObjectPublicationOutcome::OutcomeUnknown {
                error: Some(EncryptionError::Cancelled.into()),
            }),
        };
        if let Ok(response) = response {
            if let Err(error) = validate_protocol(&response) {
                return Ok(ObjectPublicationOutcome::OutcomeUnknown { error: Some(error) });
            }
            if response.status() == reqwest::StatusCode::NO_CONTENT {
                if response.content_length().is_some_and(|n| n != 0) {
                    return Ok(ObjectPublicationOutcome::OutcomeUnknown {
                        error: Some(EncryptedHttpError::ProtocolMismatch),
                    });
                }
                return Ok(ObjectPublicationOutcome::Created);
            }
        }
        match self.object_header(&operation, candidate.remote_key()).await {
            Ok(Some(reader)) => {
                if reader.logical_key() != candidate.snapshot.logical_key
                    || reader.plaintext_length() != candidate.snapshot.plaintext_length
                    || reader.plaintext_digest() != &candidate.snapshot.plaintext_sha256
                {
                    return Err(EncryptionError::ImmutableConflict.into());
                }
                Ok(ObjectPublicationOutcome::Reused)
            }
            Ok(None) => Ok(ObjectPublicationOutcome::Unconfirmed),
            Err(error) => Ok(ObjectPublicationOutcome::OutcomeUnknown { error: Some(error) }),
        }
    }

    fn object_url(&self, operation: &str, key: &str) -> Result<reqwest::Url> {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| EncryptedHttpError::InvalidEndpoint)?;
        path.pop_if_empty().push(operation);
        if !key.is_empty() {
            for segment in key.split('/') {
                path.push(segment);
            }
        }
        drop(path);
        Ok(url)
    }

    /// Authenticate a listed object's header without downloading its payload.
    pub async fn discover_object(&self, remote: &str) -> Result<Option<EncryptedObjectMetadata>> {
        object::opaque_class(remote)?;
        let operation = self.operation().await?;
        let Some(reader) = self.object_header(&operation, remote).await? else {
            return Ok(None);
        };
        let metadata = EncryptedObjectMetadata {
            logical_key: reader.logical_key().to_owned(),
            plaintext_length: reader.plaintext_length(),
            plaintext_sha256: *reader.plaintext_digest(),
        };
        operation.check_active()?;
        Ok(Some(metadata))
    }

    /// Immutable listing identity only, never a fresh existence or payload proof.
    pub(crate) async fn discover_object_cached(
        &self,
        remote: &str,
    ) -> Result<Option<EncryptedObjectMetadata>> {
        object::opaque_class(remote)?;
        let operation = self.operation().await?;
        let reader = if let Some((header, total)) =
            operation.cached_discovery_header(self.base.as_str(), remote)?
        {
            ObjectReader::discover(operation.root()?, remote, &header, total)?
        } else {
            let Some((reader, header, total)) =
                self.fetch_object_header(&operation, remote).await?
            else {
                return Ok(None);
            };
            operation.cache_discovery_header(self.base.as_str(), remote, header, total)?;
            reader
        };
        let metadata = EncryptedObjectMetadata {
            logical_key: reader.logical_key().to_owned(),
            plaintext_length: reader.plaintext_length(),
            plaintext_sha256: *reader.plaintext_digest(),
        };
        operation.check_active()?;
        Ok(Some(metadata))
    }

    /// Authenticate covering blocks before returning a bounded plaintext range.
    /// Partial reads do not establish the complete object's plaintext digest.
    pub async fn read_object_range(
        &self,
        logical: &str,
        range: Range<u64>,
    ) -> Result<Option<Vec<u8>>> {
        let length = range
            .end
            .checked_sub(range.start)
            .ok_or(EncryptionError::InvalidChunk)?;
        if length > MAX_RANGE_PLAINTEXT {
            return Err(EncryptionError::TooLarge.into());
        }
        let operation = self.operation().await?;
        let remote = object::remote_key(operation.root()?, logical)?;
        let Some(reader) = self.object_header(&operation, &remote).await? else {
            return Ok(None);
        };
        if reader.logical_key() != logical {
            return Err(EncryptionError::AuthenticationFailed.into());
        }
        let covered = reader.ciphertext_range(range.clone())?;
        if range.is_empty() {
            operation.check_active()?;
            return Ok(Some(Vec::new()));
        }
        let (bytes, total) = self
            .get_range(&operation, &remote, covered.clone(), false)
            .await?
            .ok_or(EncryptedHttpError::ProtocolMismatch)?;
        if total != reader.chunk_range(reader.chunk_count() - 1)?.end {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        let output = decrypt_range(&operation, &reader, range, covered.start, &bytes)?;
        operation.check_active()?;
        Ok(Some(output))
    }

    /// Download one full object without whole-object buffering or per-block requests.
    /// `directory` is the host's private local cache, outside the worktree and sync.
    /// No file handle is exposed before all chunks, framing and the digest pass.
    pub async fn download_object(
        &self,
        logical: &str,
        directory: &Path,
        maximum_plaintext: u64,
    ) -> Result<Option<DownloadedEncryptedObject>> {
        let operation = self.operation().await?;
        let remote = object::remote_key(operation.root()?, logical)?;
        let Some(reader) = self.object_header(&operation, &remote).await? else {
            return Ok(None);
        };
        if reader.logical_key() != logical {
            return Err(EncryptionError::AuthenticationFailed.into());
        }
        if reader.plaintext_length() > maximum_plaintext {
            return Err(EncryptionError::TooLarge.into());
        }
        let file = private_staging_file(directory)?;
        let mut file = tokio::fs::File::from_std(file);
        let range = reader.chunk_range(0)?.start..reader.chunk_range(reader.chunk_count() - 1)?.end;
        let (response, total) = self
            .range_response(&operation, &remote, range.clone(), false)
            .await?
            .ok_or(EncryptedHttpError::ProtocolMismatch)?;
        if total != range.end {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        let mut body = CiphertextBody::new(response, range.end - range.start);
        stream_plaintext(&operation, &reader, &mut body, &mut file).await?;
        let file = finish_staging(&operation, file).await?;
        operation.check_active()?;
        Ok(Some(DownloadedEncryptedObject {
            file,
            metadata: EncryptedObjectMetadata {
                logical_key: logical.to_owned(),
                plaintext_length: reader.plaintext_length(),
                plaintext_sha256: *reader.plaintext_digest(),
            },
        }))
    }

    /// One bounded aggregate request. Files are handed off only after the whole batch passes.
    /// Public hints authenticate association, but require actual pack topology verification.
    pub async fn read_bundle(
        &self,
        keys: &[String],
        directory: &Path,
        maximum_plaintext: u64,
    ) -> Result<Vec<EncryptedBundleEntry>> {
        let keys = bundle::prepare_keys(keys)?;
        let body = serde_json::to_vec(&serde_json::json!({"version":1,"keys":keys}))
            .map_err(|_| EncryptedHttpError::ProtocolMismatch)?;
        let operation = self.operation().await?;
        self.ensure_request_budget(&operation, body.len() as u64)
            .await?;
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| EncryptedHttpError::InvalidEndpoint)?
            .pop_if_empty()
            .push("e2ee")
            .push("read-bundle");
        let request = self.credentials.authorize_encrypted_request(
            &self.remote_name,
            self.client
                .post(url)
                .header("Graft-Protocol", "1")
                .header("Content-Type", "application/json")
                .header("Accept-Encoding", "identity")
                .header("Content-Length", body.len())
                .body(body),
        );
        let response = tokio::select! {
            result = request.send() => result.map_err(transport_error)?,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        operation.check_active()?;
        let total = validate_bundle_headers(&response, keys.len())?;
        bundle::read(
            &operation,
            response,
            &keys,
            total,
            directory,
            maximum_plaintext,
        )
        .await
    }

    async fn object_header(
        &self,
        operation: &SessionOperation,
        remote: &str,
    ) -> Result<Option<ObjectReader>> {
        Ok(self
            .fetch_object_header(operation, remote)
            .await?
            .map(|(reader, _, _)| reader))
    }

    async fn fetch_object_header(
        &self,
        operation: &SessionOperation,
        remote: &str,
    ) -> Result<Option<(ObjectReader, Vec<u8>, u64)>> {
        let Some((mut prefix, total)) = self
            .get_range(operation, remote, 0..object::PREFIX_LENGTH as u64, true)
            .await?
        else {
            return Ok(None);
        };
        let header_length = ObjectReader::header_length(&prefix)?;
        if total < header_length as u64 + 16 {
            return Err(EncryptionError::InvalidEncoding.into());
        }
        let (header, observed_total) = self
            .get_range(
                operation,
                remote,
                object::PREFIX_LENGTH as u64..header_length as u64,
                false,
            )
            .await?
            .ok_or(EncryptedHttpError::ProtocolMismatch)?;
        if total != observed_total {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        prefix.extend_from_slice(&header);
        let reader = ObjectReader::discover(operation.root()?, remote, &prefix, total)?;
        operation.check_active()?;
        Ok(Some((reader, prefix, total)))
    }

    fn raw_url(&self, key: &str) -> Result<reqwest::Url> {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| EncryptedHttpError::InvalidEndpoint)?;
        path.pop_if_empty().push("raw");
        for segment in key.split('/') {
            path.push(segment);
        }
        drop(path);
        Ok(url)
    }

    async fn get_range(
        &self,
        operation: &SessionOperation,
        key: &str,
        range: Range<u64>,
        missing: bool,
    ) -> Result<Option<(Vec<u8>, u64)>> {
        operation.check_active()?;
        let length = range
            .end
            .checked_sub(range.start)
            .filter(|n| *n > 0)
            .ok_or(EncryptionError::InvalidChunk)?;
        // One plaintext request is bounded; covering blocks add at most two chunks and tags.
        if length > MAX_RANGE_PLAINTEXT + 1024 * 1024 {
            return Err(EncryptionError::TooLarge.into());
        }
        let Some((response, total)) = self.range_response(operation, key, range, missing).await?
        else {
            return Ok(None);
        };
        let bytes = read_body(operation, response, length as usize).await?;
        if bytes.len() as u64 != length {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        Ok(Some((bytes, total)))
    }

    async fn range_response(
        &self,
        operation: &SessionOperation,
        key: &str,
        range: Range<u64>,
        missing: bool,
    ) -> Result<Option<(reqwest::Response, u64)>> {
        operation.check_active()?;
        if range.end <= range.start {
            return Err(EncryptionError::InvalidChunk.into());
        }
        let request = self.credentials.authorize_encrypted_request(
            &self.remote_name,
            self.client
                .get(self.raw_url(key)?)
                .header("Graft-Protocol", "1")
                .header("Accept-Encoding", "identity")
                .header("Range", format!("bytes={}-{}", range.start, range.end - 1)),
        );
        let response = tokio::select! {
            result = request.send() => result.map_err(transport_error)?,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        operation.check_active()?;
        validate_protocol(&response)?;
        if missing && response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(EncryptedHttpError::HttpStatus(response.status().as_u16()));
        }
        let total = validate_content_range(&response, &range)?;
        Ok(Some((response, total)))
    }

    pub(crate) async fn operation(&self) -> Result<SessionOperation> {
        let mut operation = self.session.begin_operation()?;
        let epoch = operation.epoch()?;
        let mut verified = tokio::select! {
            guard = self.verified.lock() => guard,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        if verified.as_ref().map(|profile| profile.epoch) != Some(epoch) {
            let descriptor = self
                .get(&operation, None, 256 * 1024, false)
                .await?
                .ok_or(EncryptedHttpError::UnsupportedProfile)?;
            let limits = validate_descriptor(&descriptor, operation.root()?.repository())?;
            let bytes = self
                .get(&operation, Some("e2ee/config"), 206, true)
                .await?
                .ok_or(EncryptedHttpError::ConfigMissing)?;
            let config = EncryptedConfig::parse(&bytes)?;
            operation = operation.observe_config_durable(config).await?;
            *verified = Some(VerifiedProfile { epoch, limits });
        }
        operation.check_active()?;
        Ok(operation)
    }

    pub(crate) fn session(&self) -> &SecretSession {
        &self.session
    }

    /// Publish a locally authenticated immutable routing hint. The caller must
    /// verify its topology against actual pack objects before invoking this method.
    pub(crate) async fn publish_hint(&self, hint: &super::RoutingHint) -> Result<()> {
        let operation = self.operation().await?;
        super::RoutingHint::open(
            operation.root()?,
            hint.pack_key(),
            hint.index_key(),
            hint.as_bytes(),
        )?;
        self.ensure_request_budget(&operation, hint.as_bytes().len() as u64)
            .await?;
        for _ in 0..3 {
            let request = self.credentials.authorize_encrypted_request(
                &self.remote_name,
                self.client
                    .put(self.object_url("raw-if-not-exists", hint.remote_key())?)
                    .header("Graft-Protocol", "1")
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Length", hint.as_bytes().len())
                    .body(hint.as_bytes().to_vec()),
            );
            operation.check_active()?;
            let response = tokio::select! {
                response = request.send() => response.map_err(transport_error),
                _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
            };
            if let Ok(response) = response {
                validate_protocol(&response)?;
                if response.status() == reqwest::StatusCode::NO_CONTENT {
                    return Ok(());
                }
            }
            if let Some(existing) = self
                .get(&operation, Some(hint.remote_key()), 1024 * 1024, true)
                .await?
            {
                super::RoutingHint::open(
                    operation.root()?,
                    hint.pack_key(),
                    hint.index_key(),
                    &existing,
                )?;
                if existing != hint.as_bytes() {
                    return Err(EncryptionError::ImmutableConflict.into());
                }
                return Ok(());
            }
        }
        Err(EncryptedHttpError::ProtocolMismatch)
    }

    async fn get(
        &self,
        operation: &SessionOperation,
        key: Option<&str>,
        maximum: usize,
        missing: bool,
    ) -> Result<Option<Vec<u8>>> {
        operation.check_task_active()?;
        let url = match key {
            Some(key) => self.raw_url(key)?,
            None => self.base.clone(),
        };
        let request = self.credentials.authorize_encrypted_request(
            &self.remote_name,
            self.client
                .get(url)
                .header("Graft-Protocol", "1")
                .timeout(Duration::from_secs(60).min(self.transfer_timeout)),
        );
        let response = tokio::select! {
            result = request.send() => result.map_err(transport_error)?,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        read_response(operation, response, maximum, missing).await
    }
}

pub(crate) fn validate_endpoint(url: &str) -> Result<reqwest::Url> {
    let mut base = reqwest::Url::parse(url).map_err(|_| EncryptedHttpError::InvalidEndpoint)?;
    let loopback = matches!(base.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if base.scheme() != "https" && !(base.scheme() == "http" && loopback)
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || base.cannot_be_a_base()
        || base.host_str().is_none()
    {
        return Err(EncryptedHttpError::InvalidEndpoint);
    }
    let path = base.path().trim_end_matches('/').to_owned();
    base.set_path(&path);
    Ok(base)
}

fn transfer_client(timeout: Duration) -> Result<reqwest::Client> {
    if timeout.is_zero() || std::time::Instant::now().checked_add(timeout).is_none() {
        return Err(EncryptionError::TooLarge.into());
    }
    reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .http1_only()
        .hickory_dns(!cfg!(target_os = "android"))
        .pool_max_idle_per_host(1)
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(60))
        .timeout(timeout)
        .build()
        .map_err(transport_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    protocol: String,
    version: u16,
    #[serde(default, rename = "repository")]
    _repository: Option<String>,
    encryption: EncryptionDescriptor,
    capabilities: HashSet<String>,
    #[serde(default)]
    limits: Limits,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EncryptionDescriptor {
    profile: String,
    version: u16,
    suite: u16,
    uuid: String,
}
#[derive(Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    max_request_bytes: Option<u64>,
    multipart_part_bytes: Option<u64>,
    #[serde(skip)]
    fetch_bundle: bool,
    #[serde(skip)]
    receive_bundle: bool,
    #[serde(skip)]
    multipart_object: bool,
}

fn validate_descriptor(bytes: &[u8], repository: super::RepositoryId) -> Result<Limits> {
    let mut descriptor: Descriptor =
        serde_json::from_slice(bytes).map_err(|_| EncryptedHttpError::UnsupportedProfile)?;
    let encryption = descriptor.encryption;
    if descriptor.protocol != "graft-remote"
        || descriptor.version != 1
        || encryption.profile != "encrypted-remote-v1"
        || encryption.version != 1
        || encryption.suite != 1
        || descriptor.limits.max_request_bytes == Some(0)
        || descriptor.limits.multipart_part_bytes == Some(0)
        || ![
            "range",
            "list",
            "list-cursor",
            "put-if-absent",
            "cas",
            "cad",
            "e2ee-read-bundle",
        ]
        .iter()
        .all(|value| descriptor.capabilities.contains(*value))
    {
        return Err(EncryptedHttpError::UnsupportedProfile);
    }
    let raw = repository.as_bytes();
    let uuid: String = raw
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            format!(
                "{}{byte:02x}",
                if [4, 6, 8, 10].contains(&index) {
                    "-"
                } else {
                    ""
                }
            )
        })
        .collect();
    if encryption.uuid != uuid {
        return Err(EncryptionError::IdentityChanged.into());
    }
    descriptor.limits.fetch_bundle = descriptor.capabilities.contains("e2ee-fetch-bundle");
    descriptor.limits.multipart_object = descriptor.capabilities.contains("e2ee-multipart-object");
    descriptor.limits.receive_bundle = descriptor.capabilities.contains("e2ee-receive-bundle");
    Ok(descriptor.limits)
}

fn validate_bundle_headers(response: &reqwest::Response, count: usize) -> Result<u64> {
    validate_protocol(response)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(EncryptedHttpError::HttpStatus(response.status().as_u16()));
    }
    if single_header(response, "Content-Type")? != "application/vnd.graft.e2ee-bundle"
        || response.headers().contains_key("Content-Encoding")
            && single_header(response, "Content-Encoding")? != "identity"
    {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let number = |key| -> Result<u64> {
        let value = single_header(response, key)?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(EncryptedHttpError::ProtocolMismatch);
        }
        value
            .parse()
            .map_err(|_| EncryptedHttpError::ProtocolMismatch)
    };
    let total = number("x-graft-bundle-total-bytes")?;
    if total > bundle::MAX_BUNDLE_BYTES {
        return Err(EncryptionError::TooLarge.into());
    }
    if number("x-graft-bundle-objects")? != count as u64
        || total < 12
        || response
            .content_length()
            .is_some_and(|length| length != total)
    {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    Ok(total)
}

async fn read_response(
    operation: &SessionOperation,
    response: reqwest::Response,
    maximum: usize,
    missing: bool,
) -> Result<Option<Vec<u8>>> {
    operation.check_task_active()?;
    validate_protocol(&response)?;
    if missing && response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if response.status() != reqwest::StatusCode::OK {
        return Err(EncryptedHttpError::HttpStatus(response.status().as_u16()));
    }
    Ok(Some(read_body(operation, response, maximum).await?))
}

fn single_header<'a>(response: &'a reqwest::Response, key: &str) -> Result<&'a str> {
    let mut values = response.headers().get_all(key).iter();
    let value = values.next().ok_or(EncryptedHttpError::ProtocolMismatch)?;
    if values.next().is_some() {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    value
        .to_str()
        .map_err(|_| EncryptedHttpError::ProtocolMismatch)
}

fn validate_protocol(response: &reqwest::Response) -> Result<()> {
    if single_header(response, "Graft-Protocol")? != "1" {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    Ok(())
}

fn validate_content_range(response: &reqwest::Response, range: &Range<u64>) -> Result<u64> {
    if response.headers().contains_key("Content-Encoding")
        && single_header(response, "Content-Encoding")? != "identity"
    {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    let total = parse_content_range(single_header(response, "Content-Range")?, range)?;
    if response
        .content_length()
        .is_some_and(|n| n != range.end - range.start)
    {
        return Err(EncryptedHttpError::ProtocolMismatch);
    }
    Ok(total)
}

fn parse_content_range(value: &str, range: &Range<u64>) -> Result<u64> {
    let mismatch = || EncryptedHttpError::ProtocolMismatch;
    let (bounds, total) = value
        .strip_prefix("bytes ")
        .ok_or_else(mismatch)?
        .split_once('/')
        .ok_or_else(mismatch)?;
    let (start, end) = bounds.split_once('-').ok_or_else(mismatch)?;
    let number = |text: &str| -> Result<u64> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(mismatch());
        }
        text.parse().map_err(|_| mismatch())
    };
    let (start, end, total) = (number(start)?, number(end)?, number(total)?);
    if start != range.start || end.checked_add(1) != Some(range.end) || total <= end {
        return Err(mismatch());
    }
    Ok(total)
}

fn decrypt_range(
    operation: &SessionOperation,
    reader: &ObjectReader,
    range: Range<u64>,
    base: u64,
    bytes: &[u8],
) -> Result<Vec<u8>> {
    let first = range.start / reader.chunk_size();
    let last = (range.end - 1) / reader.chunk_size();
    let mut output = zeroize::Zeroizing::new(Vec::new());
    for index in first..=last {
        operation.check_active()?;
        let covered = reader.chunk_range(index)?;
        let start = usize::try_from(covered.start - base).map_err(|_| EncryptionError::TooLarge)?;
        let end = usize::try_from(covered.end - base).map_err(|_| EncryptionError::TooLarge)?;
        let ciphertext = bytes
            .get(start..end)
            .ok_or(EncryptedHttpError::ProtocolMismatch)?;
        let chunk = zeroize::Zeroizing::new(reader.decrypt_chunk(index, ciphertext)?);
        let plaintext_start = index * reader.chunk_size();
        let from = range.start.saturating_sub(plaintext_start) as usize;
        let to = (range.end - plaintext_start).min(chunk.len() as u64) as usize;
        output.extend_from_slice(&chunk[from..to]);
    }
    if range.start == 0
        && range.end == reader.plaintext_length()
        && Sha256::digest(&*output).as_slice() != reader.plaintext_digest()
    {
        return Err(EncryptionError::SnapshotMismatch.into());
    }
    operation.check_active()?;
    Ok(std::mem::take(&mut *output))
}

async fn read_body(
    operation: &SessionOperation,
    mut response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>> {
    operation.check_task_active()?;
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(EncryptionError::TooLarge.into());
    }
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            result = response.chunk() => result.map_err(transport_error)?,
            _ = operation.cancelled() => return Err(EncryptionError::Cancelled.into()),
        };
        let Some(chunk) = chunk else {
            break;
        };
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > maximum)
        {
            return Err(EncryptionError::TooLarge.into());
        }
        bytes.extend_from_slice(&chunk);
    }
    operation.check_task_active()?;
    Ok(bytes)
}

fn transport_error(error: reqwest::Error) -> EncryptedHttpError {
    EncryptedHttpError::Transport(error.without_url())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2ee::{EncryptedRef, SecretSessionState, SessionBinding, tests::fixture_root};
    use std::sync::OnceLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Download fixtures obey the same private staging policy as trusted hosts.
    fn private_tempdir() -> tempfile::TempDir {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        crate::native_fs::set_permissions(
            directory.path(),
            crate::native_fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        directory
    }

    static CONFIG: OnceLock<EncryptedConfig> = OnceLock::new();
    fn config() -> EncryptedConfig {
        CONFIG
            .get_or_init(|| {
                EncryptedConfig::create(&fixture_root(), b"http sync password").unwrap()
            })
            .clone()
    }
    fn locked_session() -> SecretSession {
        SecretSession::new(
            SessionBinding::new(
                "native.test".into(),
                "test-account".into(),
                fixture_root().repository(),
            )
            .unwrap(),
            None,
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn persistent_session() -> (tempfile::TempDir, SecretSession) {
        use std::os::unix::fs::PermissionsExt;
        let directory = private_tempdir();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let binding = SessionBinding::new(
            "native.test".into(),
            "test-account".into(),
            fixture_root().repository(),
        )
        .unwrap();
        let store = crate::e2ee::FileCheckpointStore::create(directory.path(), &binding).unwrap();
        let session = SecretSession::with_checkpoint_store(binding, Arc::new(store)).unwrap();
        (directory, session)
    }
    fn session() -> SecretSession {
        let session = locked_session();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        session
    }
    fn descriptor() -> serde_json::Value {
        serde_json::json!({"protocol":"graft-remote", "version":1,
            "encryption":{"profile":"encrypted-remote-v1","version":1,"suite":1,"uuid":"00010203-0405-4607-8809-0a0b0c0d0e0f"},
            "capabilities":["range","list","list-cursor","put-if-absent","cas","cad","e2ee-read-bundle"]})
    }
    struct Reply {
        status: u16,
        headers: String,
        bytes: Vec<u8>,
    }
    fn reply(bytes: Vec<u8>) -> Reply {
        Reply {
            status: 200,
            headers: "Graft-Protocol: 1\r\n".into(),
            bytes,
        }
    }
    async fn server(replies: Vec<Reply>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut stream).await);
                if reply.status == 0 {
                    continue;
                }
                let header = format!(
                    "HTTP/1.1 {} Test\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.status,
                    reply.headers,
                    reply.bytes.len()
                );
                stream.write_all(header.as_bytes()).await.unwrap();
                stream.write_all(&reply.bytes).await.unwrap();
            }
            requests
        });
        (format!("http://{address}/alice/repo"), task)
    }
    fn client(url: &str, session: SecretSession) -> EncryptedHttpClient {
        let credentials = RemoteCredentials::explicit();
        credentials
            .set_http_bearer_token("origin", "account-token".into())
            .unwrap();
        EncryptedHttpClient::new(url, "origin".into(), credentials, session).unwrap()
    }

    #[tokio::test]
    async fn total_transfer_timeout_stops_a_peer_that_keeps_sending_bytes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .await
                .unwrap();
            for _ in 0..100 {
                if stream.write_all(b"x").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let client = transfer_client(Duration::from_millis(150)).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            client
                .get(format!("http://{address}/"))
                .send()
                .await
                .unwrap()
                .bytes()
                .await
        })
        .await
        .expect("an active slow peer must not defeat the total deadline");
        assert!(result.unwrap_err().is_timeout());
        serving.abort();
    }

    #[test]
    fn descriptor_rejects_unknown_control_fields() {
        let repository = fixture_root().repository();
        let mut supported = descriptor();
        supported["repository"] = "acme/repository".into();
        assert!(validate_descriptor(&serde_json::to_vec(&supported).unwrap(), repository).is_ok());
        let mut value = descriptor();
        value["unexpected"] = true.into();
        assert!(validate_descriptor(&serde_json::to_vec(&value).unwrap(), repository).is_err());
        let mut value = descriptor();
        value["encryption"]["unexpected"] = true.into();
        assert!(validate_descriptor(&serde_json::to_vec(&value).unwrap(), repository).is_err());
        let mut value = descriptor();
        value["limits"] = serde_json::json!({"unexpected":1});
        assert!(validate_descriptor(&serde_json::to_vec(&value).unwrap(), repository).is_err());
    }

    #[tokio::test]
    async fn total_transfer_timeout_also_covers_a_streamed_upload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let body = futures::stream::unfold(0_u32, |index| async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Some((Ok::<_, std::io::Error>(vec![1_u8]), index + 1))
        });
        let client = transfer_client(Duration::from_millis(150)).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            client
                .post(format!("http://{address}/"))
                .body(reqwest::Body::wrap_stream(body))
                .send(),
        )
        .await
        .expect("the upload must obey the total deadline");
        assert!(result.unwrap_err().is_timeout());
        serving.abort();
    }

    #[tokio::test]
    async fn fetch_bundle_accepts_authenticated_annotated_tag_refs() {
        let target = RefTarget::AnnotatedTag { object: [7; 32], commit: [9; 32] };
        let reference =
            super::super::EncryptedRef::seal(&fixture_root(), "refs/tags/release", &target)
                .unwrap();
        let frames = super::super::codec::encode(&[]).unwrap();
        let manifest = super::super::codec::encode(&[
            &1_u16.to_be_bytes(),
            reference.remote_key().as_bytes(),
            reference.as_bytes(),
            &frames,
        ])
        .unwrap();
        let mut body = b"GRAFTEB1".to_vec();
        body.extend_from_slice(&(manifest.len() as u32).to_be_bytes());
        body.extend_from_slice(&manifest);
        let mut descriptor = descriptor();
        descriptor["capabilities"]
            .as_array_mut()
            .unwrap()
            .push("e2ee-fetch-bundle".into());
        let mut bundled = reply(body);
        bundled.headers.push_str(&format!("Content-Type: application/vnd.graft.e2ee-bundle\r\nx-graft-bundle-objects: 0\r\nx-graft-bundle-total-bytes: {}\r\n", bundled.bytes.len()));
        let (url, requests) = server(vec![
            reply(serde_json::to_vec(&descriptor).unwrap()),
            reply(config().as_bytes().to_vec()),
            bundled,
        ])
        .await;
        let directory = private_tempdir();
        let fetched = client(&url, session())
            .fetch_bundle("refs/tags/release", None, directory.path(), 1024)
            .await
            .unwrap()
            .unwrap();
        assert!(fetched.reference.target() == Some(&target));
        assert!(fetched.entries.is_empty());
        assert_eq!(requests.await.unwrap().len(), 3);
    }

    #[cfg(unix)]
    fn initial_journal(
        session: &SecretSession,
    ) -> (
        tempfile::TempDir,
        Arc<super::super::FileInitializationJournal>,
    ) {
        use std::os::unix::fs::PermissionsExt;
        let directory = private_tempdir();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = super::super::FileInitializationJournal::create(
            directory.path(),
            session.binding(),
            &config(),
        )
        .unwrap();
        (directory, Arc::new(journal))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_mutation_publication_retains_exact_candidates_and_reconciles_outcomes() {
        use crate::e2ee::{
            CheckpointStore, ConfigMutationJournal, ConfigMutationRecord,
            ConfigPublicationOutcome as Outcome, FileCheckpointStore, FileConfigMutationJournal,
        };
        use std::os::unix::fs::PermissionsExt;
        struct LostRetirementAck {
            journal: Arc<FileConfigMutationJournal>,
            fail: std::sync::atomic::AtomicBool,
        }
        impl ConfigMutationJournal for LostRetirementAck {
            fn load(
                &self,
                binding: &SessionBinding,
            ) -> super::super::Result<Option<ConfigMutationRecord>> {
                self.journal.load(binding)
            }
            fn load_recoverable(
                &self,
                binding: &SessionBinding,
            ) -> super::super::Result<Option<ConfigMutationRecord>> {
                self.journal.load_recoverable(binding)
            }
            fn bind(
                &self,
                binding: &SessionBinding,
                record: &ConfigMutationRecord,
            ) -> super::super::Result<()> {
                self.journal.bind(binding, record)
            }
            fn retire(
                &self,
                binding: &SessionBinding,
                record: &ConfigMutationRecord,
            ) -> super::super::Result<()> {
                self.journal.retire(binding, record)?;
                if self.fail.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::Other,
                    ));
                }
                Ok(())
            }
        }
        let next = config()
            .password_change_candidate(&fixture_root(), b"changed synthetic password")
            .unwrap();
        let other = config()
            .password_change_candidate(&fixture_root(), b"concurrent synthetic password")
            .unwrap();
        let record = ConfigMutationRecord::from_local_bytes(
            &crate::e2ee::codec::encode(&[config().as_bytes(), next.as_bytes()]).unwrap(),
        )
        .unwrap();
        for (status, observed, expected) in [
            (204, 0, 0),
            (0, 0, 0),
            (500, 1, 1),
            (409, 0, 0),
            (409, 1, 1),
            (409, 2, 2),
            (412, 2, 2),
            (0, 2, 3),
            (204, 3, 3),
            (204, 4, 3),
            (204, 0, 4),
        ] {
            let (checkpoint_directory, session) = persistent_session();
            session.unlock_with_root(&config(), fixture_root()).unwrap();
            let candidate = session
                .begin_operation()
                .unwrap()
                .restore_config_mutation(record.clone())
                .unwrap();
            let directory = private_tempdir();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let journal = Arc::new(
                FileConfigMutationJournal::create(directory.path(), session.binding()).unwrap(),
            );
            let observed = match observed {
                0 => reply(next.as_bytes().to_vec()),
                1 => reply(config().as_bytes().to_vec()),
                2 => reply(other.as_bytes().to_vec()),
                3 => reply(vec![0; 206]),
                _ => Reply {
                    status: 404,
                    headers: "Graft-Protocol: 1\r\n".into(),
                    bytes: vec![],
                },
            };
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
                Reply {
                    status,
                    headers: "Graft-Protocol: 1\r\n".into(),
                    bytes: vec![],
                },
                observed,
            ])
            .await;
            let outcome = client(&url, session.clone())
                .publish_config_mutation(
                    &candidate,
                    Arc::new(LostRetirementAck {
                        journal: journal.clone(),
                        fail: std::sync::atomic::AtomicBool::new(expected == 4),
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                (outcome, expected),
                (Outcome::Confirmed, 0)
                    | (Outcome::Unconfirmed, 1)
                    | (Outcome::Conflict, 2)
                    | (Outcome::OutcomeUnknown { .. }, 3)
                    | (Outcome::OutcomeUnknown { .. }, 4)
            ));
            let reopened =
                FileConfigMutationJournal::open(directory.path(), session.binding()).unwrap();
            assert!(
                reopened.load(session.binding()).unwrap()
                    == if expected == 0 || expected == 2 || expected == 4 {
                        None
                    } else {
                        Some(record.clone())
                    }
            );
            assert!(
                session.checkpoint().unwrap()
                    == if expected == 0 || expected == 4 {
                        next.checkpoint(&fixture_root()).unwrap()
                    } else {
                        config().checkpoint(&fixture_root()).unwrap()
                    }
            );
            assert!(
                FileCheckpointStore::open(checkpoint_directory.path(), session.binding())
                    .unwrap()
                    .load(session.binding())
                    .unwrap()
                    == session.checkpoint()
            );
            let requests = served.await.unwrap();
            assert_eq!(requests.len(), 4);
            assert!(requests[2].starts_with("POST /alice/repo/cas/e2ee/config "));
            let request = requests[2].to_ascii_lowercase();
            let hex: String = record
                .expected_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert!(
                request.contains("x-graft-expected-present: true")
                    && request.contains(&format!("x-graft-expected-hex: {hex}"))
            );
            if expected == 4 || expected == 2 {
                session.disconnect().await;
                let store = Arc::new(
                    FileCheckpointStore::open(checkpoint_directory.path(), session.binding())
                        .unwrap(),
                );
                let binding = SessionBinding::new(
                    "native.test".into(),
                    "test-account".into(),
                    fixture_root().repository(),
                )
                .unwrap();
                let fresh = SecretSession::with_checkpoint_store(binding, store).unwrap();
                let current = if expected == 4 { &next } else { &other };
                fresh.unlock_with_root(current, fixture_root()).unwrap();
                let journal = Arc::new(
                    FileConfigMutationJournal::open(directory.path(), fresh.binding()).unwrap(),
                );
                let (url, served) = server(vec![
                    reply(serde_json::to_vec(&descriptor()).unwrap()),
                    reply(current.as_bytes().to_vec()),
                    reply(current.as_bytes().to_vec()),
                ])
                .await;
                let fresh_client = client(&url, fresh.clone());
                let restored = fresh_client
                    .restore_config_mutation(journal.clone())
                    .await
                    .unwrap();
                assert!(restored.record(&fresh.begin_operation().unwrap()).unwrap() == record);
                let outcome = fresh_client
                    .reconcile_config_mutation(&restored, journal.clone())
                    .await
                    .unwrap();
                assert!(matches!(
                    (outcome, expected),
                    (Outcome::Confirmed, 4) | (Outcome::OutcomeUnknown { .. }, 2)
                ));
                assert!(journal.load(fresh.binding()).unwrap().is_none());
                assert!(
                    served
                        .await
                        .unwrap()
                        .iter()
                        .all(|request| request.starts_with("GET "))
                );
            }
            if expected == 1 {
                // Fresh native session/client/provider: restore retained bytes,
                // inspect without CAS, then retry the same exact replacement.
                let fresh = self::session();
                let journal = Arc::new(
                    FileConfigMutationJournal::open(directory.path(), fresh.binding()).unwrap(),
                );
                let (url, served) = server(vec![
                    reply(serde_json::to_vec(&descriptor()).unwrap()),
                    reply(config().as_bytes().to_vec()),
                    reply(config().as_bytes().to_vec()),
                    Reply {
                        status: 204,
                        headers: "Graft-Protocol: 1\r\n".into(),
                        bytes: vec![],
                    },
                    reply(next.as_bytes().to_vec()),
                ])
                .await;
                let fresh_client = client(&url, fresh.clone());
                let restored = fresh_client
                    .restore_config_mutation(journal.clone())
                    .await
                    .unwrap();
                assert!(restored.record(&fresh.begin_operation().unwrap()).unwrap() == record);
                assert!(matches!(
                    fresh_client
                        .reconcile_config_mutation(&restored, journal.clone())
                        .await
                        .unwrap(),
                    Outcome::Unconfirmed
                ));
                assert!(matches!(
                    fresh_client
                        .publish_config_mutation(&restored, journal.clone())
                        .await
                        .unwrap(),
                    Outcome::Confirmed
                ));
                assert!(journal.load(fresh.binding()).unwrap().is_none());
                let requests = served.await.unwrap();
                assert_eq!(requests.len(), 5);
                assert!(
                    requests[..3]
                        .iter()
                        .all(|request| request.starts_with("GET "))
                );
                assert!(requests[3].starts_with("POST /alice/repo/cas/e2ee/config "));
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_conflict_checks_latest_shared_checkpoint_before_retirement() {
        use crate::e2ee::{
            CheckpointStore, ConfigMutationJournal, ConfigMutationRecord, ConfigPublicationOutcome,
            FileCheckpointStore, FileConfigMutationJournal,
        };
        use std::os::unix::fs::PermissionsExt;

        let winner = config()
            .password_change_candidate(&fixture_root(), b"shared checkpoint winner")
            .unwrap();
        let fork = config()
            .password_change_candidate(&fixture_root(), b"shared checkpoint fork")
            .unwrap();
        let later = winner
            .password_change_candidate(&fixture_root(), b"shared checkpoint later")
            .unwrap();
        let candidate_config = config()
            .password_change_candidate(&fixture_root(), b"shared checkpoint candidate")
            .unwrap();
        let record = ConfigMutationRecord::from_local_bytes(
            &crate::e2ee::codec::encode(&[config().as_bytes(), candidate_config.as_bytes()])
                .unwrap(),
        )
        .unwrap();
        for (durable_config, observed, conflict) in [
            (&winner, &fork, false),
            (&later, &winner, false),
            (&winner, &winner, true),
        ] {
            let (checkpoint_directory, session) = persistent_session();
            session.unlock_with_root(&config(), fixture_root()).unwrap();
            let candidate = session
                .begin_operation()
                .unwrap()
                .restore_config_mutation(record.clone())
                .unwrap();
            let directory = private_tempdir();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let journal = Arc::new(
                FileConfigMutationJournal::create(directory.path(), session.binding()).unwrap(),
            );
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
                Reply {
                    status: 409,
                    headers: "Graft-Protocol: 1\r\n".into(),
                    bytes: vec![],
                },
                reply(observed.as_bytes().to_vec()),
            ])
            .await;
            let client = client(&url, session.clone());
            client.verify().await.unwrap();
            // A different native session has observed newer trust since this
            // client's verification. Its in-memory checkpoint remains stale.
            let store =
                FileCheckpointStore::open(checkpoint_directory.path(), session.binding()).unwrap();
            let durable = durable_config.checkpoint(&fixture_root()).unwrap();
            store.advance(session.binding(), &durable).unwrap();
            let outcome = client
                .publish_config_mutation(&candidate, journal.clone())
                .await
                .unwrap();
            if conflict {
                assert!(matches!(outcome, ConfigPublicationOutcome::Conflict));
                assert!(journal.load(session.binding()).unwrap().is_none());
            } else {
                assert!(matches!(
                    outcome,
                    ConfigPublicationOutcome::OutcomeUnknown {
                        error: Some(EncryptedHttpError::Encryption(
                            EncryptionError::ConfigRollbackDetected
                        ))
                    }
                ));
                assert!(journal.load(session.binding()).unwrap() == Some(record.clone()));
            }
            assert!(journal.load_recoverable(session.binding()).unwrap() == Some(record.clone()));
            assert_eq!(store.load(session.binding()).unwrap(), Some(durable));
            assert_eq!(
                session.checkpoint(),
                Some(config().checkpoint(&fixture_root()).unwrap())
            );
            assert_eq!(served.await.unwrap().len(), 4);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_initial_config_recovers_prepared_random_root_after_lost_readback() {
        use crate::e2ee::{
            FileCheckpointStore, InitialConfigPublicationOutcome, InitializationJournal,
            InitializationStage,
        };
        use std::os::unix::fs::PermissionsExt;
        let checkpoint = private_tempdir();
        let recovery = private_tempdir();
        for directory in [&checkpoint, &recovery] {
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let binding = || {
            SessionBinding::new(
                "native.test".into(),
                "test-account".into(),
                fixture_root().repository(),
            )
            .unwrap()
        };
        let password =
            || zeroize::Zeroizing::new(b"native initial HTTP synthetic password".to_vec());
        let session = SecretSession::with_checkpoint_store(
            binding(),
            Arc::new(FileCheckpointStore::create(checkpoint.path(), &binding()).unwrap()),
        )
        .unwrap();
        let journal = session
            .prepare_native_initial_config(password(), recovery.path().to_owned(), true)
            .unwrap()
            .await
            .unwrap();
        let config = journal.load(session.binding()).unwrap().config;
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            Reply {
                status: 204,
                headers: "Graft-Protocol: 1\r\n".into(),
                bytes: vec![],
            },
            Reply {
                status: 0,
                headers: String::new(),
                bytes: vec![],
            },
        ])
        .await;
        assert!(matches!(
            client(&url, session.clone())
                .publish_initial_config(journal.clone())
                .await
                .unwrap(),
            InitialConfigPublicationOutcome::OutcomeUnknown { .. }
        ));
        assert_eq!(
            journal.load(session.binding()).unwrap().stage,
            InitializationStage::Prepared
        );
        let requests = served.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("PUT /alice/repo/raw-if-not-exists/e2ee/config "));
        session.disconnect().await;
        drop(journal);
        let fresh = SecretSession::with_checkpoint_store(
            binding(),
            Arc::new(FileCheckpointStore::open(checkpoint.path(), &binding()).unwrap()),
        )
        .unwrap();
        let journal = fresh
            .prepare_native_initial_config(password(), recovery.path().to_owned(), false)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(
            journal.load(fresh.binding()).unwrap().config.as_bytes(),
            config.as_bytes()
        );
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config.as_bytes().to_vec()),
        ])
        .await;
        assert!(matches!(
            client(&url, fresh.clone())
                .reconcile_initial_config(journal.clone())
                .await
                .unwrap(),
            InitialConfigPublicationOutcome::Confirmed
        ));
        assert_eq!(
            journal.load(fresh.binding()).unwrap().stage,
            InitializationStage::ConfigPublished
        );
        assert!(
            served
                .await
                .unwrap()
                .iter()
                .all(|request| request.starts_with("GET "))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_mutation_checkpoint_failure_retains_candidate_until_fresh_reconciliation() {
        use crate::e2ee::{
            CheckpointStore, ConfigCheckpoint, ConfigMutationJournal, ConfigMutationRecord,
            ConfigPublicationOutcome, FileCheckpointStore, FileConfigMutationJournal,
        };
        use std::os::unix::fs::PermissionsExt;
        struct FailedAdvance {
            store: Arc<FileCheckpointStore>,
            persist_before_error: bool,
            target: ConfigCheckpoint,
        }
        impl CheckpointStore for FailedAdvance {
            fn load(
                &self,
                binding: &SessionBinding,
            ) -> crate::e2ee::Result<Option<ConfigCheckpoint>> {
                self.store.load(binding)
            }
            fn advance(
                &self,
                binding: &SessionBinding,
                checkpoint: &ConfigCheckpoint,
            ) -> crate::e2ee::Result<()> {
                if checkpoint == &self.target {
                    if self.persist_before_error {
                        self.store.advance(binding, checkpoint)?;
                    }
                    return Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::Other,
                    ));
                }
                self.store.advance(binding, checkpoint)
            }
        }
        let binding = || {
            SessionBinding::new(
                "native.test".into(),
                "test-account".into(),
                fixture_root().repository(),
            )
            .unwrap()
        };
        let next = config()
            .password_change_candidate(&fixture_root(), b"checkpoint failure synthetic password")
            .unwrap();
        let record = ConfigMutationRecord::from_local_bytes(
            &crate::e2ee::codec::encode(&[config().as_bytes(), next.as_bytes()]).unwrap(),
        )
        .unwrap();
        let previous = config().checkpoint(&fixture_root()).unwrap();
        let replacement = next.checkpoint(&fixture_root()).unwrap();
        for persist_before_error in [false, true] {
            let checkpoint_directory = private_tempdir();
            let journal_directory = private_tempdir();
            for directory in [&checkpoint_directory, &journal_directory] {
                std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            let store = Arc::new(
                FileCheckpointStore::create(checkpoint_directory.path(), &binding()).unwrap(),
            );
            let session = SecretSession::with_checkpoint_store(
                binding(),
                Arc::new(FailedAdvance {
                    store: store.clone(),
                    persist_before_error,
                    target: replacement.clone(),
                }),
            )
            .unwrap();
            session.unlock_with_root(&config(), fixture_root()).unwrap();
            let journal = Arc::new(
                FileConfigMutationJournal::create(journal_directory.path(), session.binding())
                    .unwrap(),
            );
            let candidate = session
                .begin_operation()
                .unwrap()
                .restore_config_mutation(record.clone())
                .unwrap();
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
                Reply {
                    status: 204,
                    headers: "Graft-Protocol: 1\r\n".into(),
                    bytes: vec![],
                },
                reply(next.as_bytes().to_vec()),
            ])
            .await;
            let result = client(&url, session.clone())
                .publish_config_mutation(&candidate, journal.clone())
                .await
                .unwrap();
            assert!(matches!(
                result,
                ConfigPublicationOutcome::OutcomeUnknown {
                    error: Some(EncryptedHttpError::Encryption(
                        EncryptionError::CheckpointUnavailable(_)
                    ))
                }
            ));
            assert_eq!(session.checkpoint(), Some(previous.clone()));
            assert!(journal.load(session.binding()).unwrap() == Some(record.clone()));
            let durable =
                FileCheckpointStore::open(checkpoint_directory.path(), session.binding()).unwrap();
            assert_eq!(
                durable.load(session.binding()).unwrap(),
                Some(if persist_before_error {
                    replacement.clone()
                } else {
                    previous.clone()
                })
            );
            let requests = served.await.unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.starts_with("POST "))
                    .count(),
                1
            );
            session.disconnect().await;
            drop(journal);
            drop(store);

            // Fresh native providers re-confirm either the old or already-written
            // checkpoint; the still-pending exact candidate is never replaced.
            let fresh = SecretSession::with_checkpoint_store(
                binding(),
                Arc::new(
                    FileCheckpointStore::open(checkpoint_directory.path(), &binding()).unwrap(),
                ),
            )
            .unwrap();
            fresh.unlock_with_root(&next, fixture_root()).unwrap();
            let journal = Arc::new(
                FileConfigMutationJournal::open(journal_directory.path(), fresh.binding()).unwrap(),
            );
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(next.as_bytes().to_vec()),
                reply(next.as_bytes().to_vec()),
            ])
            .await;
            let client = client(&url, fresh.clone());
            let restored = client
                .restore_config_mutation(journal.clone())
                .await
                .unwrap();
            assert!(restored.record(&fresh.begin_operation().unwrap()).unwrap() == record);
            assert!(matches!(
                client
                    .reconcile_config_mutation(&restored, journal.clone())
                    .await
                    .unwrap(),
                ConfigPublicationOutcome::Confirmed
            ));
            assert_eq!(fresh.checkpoint(), Some(replacement.clone()));
            assert_eq!(
                FileCheckpointStore::open(checkpoint_directory.path(), fresh.binding())
                    .unwrap()
                    .load(fresh.binding())
                    .unwrap(),
                Some(replacement.clone())
            );
            assert!(journal.load(fresh.binding()).unwrap().is_none());
            assert!(journal.load_recoverable(fresh.binding()).unwrap() == Some(record.clone()));
            let requests = served.await.unwrap();
            assert_eq!(requests.len(), 3);
            assert!(requests.iter().all(|request| request.starts_with("GET ")));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn password_change_preparation_reads_fresh_config_without_publishing() {
        let current = config()
            .password_change_candidate(&fixture_root(), b"http fresh old password")
            .unwrap();
        let session = session();
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            reply(current.as_bytes().to_vec()),
        ])
        .await;
        let prepared = client(&url, session.clone())
            .prepare_password_change(
                zeroize::Zeroizing::new(b"http fresh old password".to_vec()),
                zeroize::Zeroizing::new(b"http fresh new password".to_vec()),
            )
            .await
            .unwrap();
        let operation = session.begin_operation().unwrap();
        let record = prepared.record(&operation).unwrap();
        assert_eq!(record.expected_bytes(), current.as_bytes());
        let replacement = EncryptedConfig::parse(record.replacement_bytes()).unwrap();
        assert_eq!(replacement.generation(), 3);
        replacement
            .unlock(
                b"http fresh new password",
                Some(fixture_root().repository()),
            )
            .unwrap();
        // Checkpoint observes current generation 2, never the unpublished generation 3.
        operation.observe_config(&current).unwrap();
        let requests = served.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| request.starts_with("GET ")
            && !request.contains("http fresh old password")
            && !request.contains("http fresh new password")));
    }

    #[tokio::test]
    async fn password_reset_requires_fresh_native_authorization_and_authenticated_config() {
        struct Authorizer {
            binding: SessionBinding,
            calls: std::sync::atomic::AtomicUsize,
            denied: bool,
        }
        impl super::super::PasswordResetAuthorizer for Authorizer {
            fn authorize(&self, binding: &SessionBinding) -> super::super::Result<()> {
                assert_eq!(binding.domain(), self.binding.domain());
                assert_eq!(binding.account(), self.binding.account());
                assert_eq!(binding.repository(), self.binding.repository());
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if self.denied {
                    Err(EncryptionError::AuthenticationFailed)
                } else {
                    Ok(())
                }
            }
        }
        let current = config()
            .password_change_candidate(&fixture_root(), b"forgotten password")
            .unwrap();
        for scenario in 0..3 {
            let session = session();
            let authorizer = Arc::new(Authorizer {
                binding: SessionBinding::new(
                    session.binding().domain().into(),
                    session.binding().account().into(),
                    session.binding().repository(),
                )
                .unwrap(),
                calls: std::sync::atomic::AtomicUsize::new(0),
                denied: scenario == 1,
            });
            let mut bytes = current.as_bytes().to_vec();
            if scenario == 2 {
                *bytes.last_mut().unwrap() ^= 1;
            }
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
                reply(bytes),
            ])
            .await;
            let result = client(&url, session.clone())
                .prepare_password_reset(
                    zeroize::Zeroizing::new(b"reset new password".to_vec()),
                    authorizer.clone(),
                )
                .await;
            if scenario == 0 {
                let operation = session.begin_operation().unwrap();
                let record = result.unwrap().record(&operation).unwrap();
                assert_eq!(record.expected_bytes(), current.as_bytes());
                let replacement = EncryptedConfig::parse(record.replacement_bytes()).unwrap();
                assert_eq!(replacement.generation(), 3);
                let root = replacement
                    .unlock(b"reset new password", Some(fixture_root().repository()))
                    .unwrap();
                current.authenticate(&root).unwrap();
                assert!(replacement.unlock(b"forgotten password", None).is_err());
                operation.observe_config(&current).unwrap();
            } else {
                assert!(matches!(
                    result,
                    Err(EncryptedHttpError::Encryption(
                        EncryptionError::AuthenticationFailed
                    ))
                ));
            }
            assert_eq!(
                authorizer.calls.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(scenario != 2)
            );
            let requests = served.await.unwrap();
            assert_eq!(requests.len(), 3);
            assert!(
                requests.iter().all(|request| request.starts_with("GET ")
                    && !request.contains("reset new password"))
            );
        }
        let session = session();
        let authorizer = Arc::new(Authorizer {
            binding: SessionBinding::new(
                session.binding().domain().into(),
                session.binding().account().into(),
                session.binding().repository(),
            )
            .unwrap(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            denied: false,
        });
        session.lock().await;
        let client = client("http://127.0.0.1:1/alice/repo", session);
        assert!(matches!(
            client
                .prepare_password_reset(
                    zeroize::Zeroizing::new(b"locked reset password".to_vec()),
                    authorizer.clone(),
                )
                .await,
            Err(EncryptedHttpError::Encryption(EncryptionError::Locked))
        ));
        assert!(matches!(
            client
                .prepare_password_reset(zeroize::Zeroizing::new(vec![0xff]), authorizer.clone(),)
                .await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::InvalidPassword
            ))
        ));
        assert_eq!(
            authorizer.calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn password_reset_authorization_stays_in_lock_drain_after_caller_abort() {
        struct Blocking {
            entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            released: std::sync::Mutex<bool>,
            wake: std::sync::Condvar,
        }
        impl super::super::PasswordResetAuthorizer for Blocking {
            fn authorize(&self, _: &SessionBinding) -> super::super::Result<()> {
                self.entered
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                let mut released = self.released.lock().unwrap();
                while !*released {
                    released = self.wake.wait(released).unwrap();
                }
                Ok(())
            }
        }
        for abort in [false, true] {
            let session = session();
            let (entered, started) = tokio::sync::oneshot::channel();
            let authorizer = Arc::new(Blocking {
                entered: std::sync::Mutex::new(Some(entered)),
                released: std::sync::Mutex::new(false),
                wake: std::sync::Condvar::new(),
            });
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
                reply(config().as_bytes().to_vec()),
            ])
            .await;
            let client = client(&url, session.clone());
            let authorization = authorizer.clone();
            let preparing = tokio::spawn(async move {
                client
                    .prepare_password_reset(
                        zeroize::Zeroizing::new(b"cancelled reset password".to_vec()),
                        authorization,
                    )
                    .await
            });
            started.await.unwrap();
            if abort {
                preparing.abort();
            }
            let locking_session = session.clone();
            let mut locking = tokio::spawn(async move { locking_session.lock().await });
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut locking)
                    .await
                    .is_err()
            );
            assert_eq!(session.state(), SecretSessionState::Locking);
            *authorizer.released.lock().unwrap() = true;
            authorizer.wake.notify_all();
            tokio::time::timeout(Duration::from_secs(2), locking)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(session.state(), SecretSessionState::Locked);
            if abort {
                assert!(matches!(preparing.await, Err(error) if error.is_cancelled()));
            } else {
                assert!(matches!(
                    preparing.await.unwrap(),
                    Err(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
                ));
            }
            assert!(
                served
                    .await
                    .unwrap()
                    .iter()
                    .all(|request| request.starts_with("GET "))
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initial_config_publication_reconciles_exact_bytes_and_retains_unknown_results() {
        use super::super::{
            InitialConfigPublicationOutcome as Outcome, InitializationJournal, InitializationStage,
        };
        let other = config()
            .password_change_candidate(&fixture_root(), b"other synthetic password")
            .unwrap();
        for (status, observed, kind) in [
            (204, 0, 0),
            (0, 0, 0),
            (412, 0, 0),
            (0, 1, 1),
            (412, 2, 2),
            (0, 2, 3),
            (204, 3, 3),
        ] {
            let session = session();
            let (_directory, journal) = initial_journal(&session);
            let readback = match observed {
                0 => reply(config().as_bytes().to_vec()),
                1 => Reply {
                    status: 404,
                    headers: "Graft-Protocol: 1\r\n".into(),
                    bytes: vec![],
                },
                2 => reply(other.as_bytes().to_vec()),
                _ => reply(vec![0; 206]),
            };
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                Reply {
                    status,
                    headers: "Graft-Protocol: 1\r\n".into(),
                    bytes: vec![],
                },
                readback,
            ])
            .await;
            let outcome = client(&url, session.clone())
                .publish_initial_config(journal.clone())
                .await
                .unwrap();
            assert!(matches!(
                (outcome, kind),
                (Outcome::Confirmed, 0)
                    | (Outcome::Unconfirmed, 1)
                    | (Outcome::Conflict, 2)
                    | (Outcome::OutcomeUnknown { .. }, 3)
            ));
            assert_eq!(
                journal.load(session.binding()).unwrap().stage,
                if kind == 0 {
                    InitializationStage::ConfigPublished
                } else {
                    InitializationStage::Prepared
                }
            );
            let requests = served.await.unwrap();
            assert_eq!(requests.len(), 3);
            assert!(
                requests[1].starts_with("PUT /alice/repo/raw-if-not-exists/e2ee/config HTTP/1.1")
            );
            let encoded = config()
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(
                requests[1].split_once("\r\n\r\n").unwrap().1,
                format!("hex:{encoded}")
            );
            assert!(requests[2].starts_with("GET /alice/repo/raw/e2ee/config HTTP/1.1"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initial_config_reconciliation_never_reinitializes_published_missing_config() {
        use super::super::{
            InitialConfigPublicationOutcome as Outcome, InitializationJournal,
            InitializationStage as Stage,
        };
        let session = session();
        let (_directory, journal) = initial_journal(&session);
        journal
            .advance(session.binding(), Stage::Prepared, Stage::ConfigPublished)
            .unwrap();
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            Reply {
                status: 404,
                headers: "Graft-Protocol: 1\r\n".into(),
                bytes: vec![],
            },
        ])
        .await;
        assert!(matches!(
            client(&url, session.clone())
                .publish_initial_config(journal.clone())
                .await
                .unwrap(),
            Outcome::OutcomeUnknown {
                error: Some(EncryptedHttpError::ConfigMissing)
            }
        ));
        assert_eq!(
            journal.load(session.binding()).unwrap().stage,
            Stage::ConfigPublished
        );
        let requests = served.await.unwrap();
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initial_config_rejects_locked_paused_foreign_root_and_profile_before_mutation() {
        let session = session();
        let (_directory, journal) = initial_journal(&session);
        let unreachable = client("http://127.0.0.1:1/alice/repo", session.clone());
        session.pause(true);
        assert!(matches!(
            unreachable.publish_initial_config(journal.clone()).await,
            Err(EncryptedHttpError::Encryption(EncryptionError::Paused))
        ));
        session.pause(false);
        session.lock().await;
        assert!(matches!(
            unreachable.publish_initial_config(journal.clone()).await,
            Err(EncryptedHttpError::Encryption(EncryptionError::Locked))
        ));
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let mut plain_descriptor = descriptor();
        plain_descriptor["encryption"]["profile"] = "plain".into();
        let (url, served) =
            server(vec![reply(serde_json::to_vec(&plain_descriptor).unwrap())]).await;
        assert!(matches!(
            client(&url, session.clone())
                .publish_initial_config(journal.clone())
                .await,
            Err(EncryptedHttpError::UnsupportedProfile)
        ));
        assert_eq!(served.await.unwrap().len(), 1);
        session.lock().await;
        let newer = config()
            .password_change_candidate(&fixture_root(), b"advanced synthetic password")
            .unwrap();
        session.unlock_with_root(&newer, fixture_root()).unwrap();
        assert!(matches!(
            unreachable.publish_initial_config(journal).await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::ConfigRollbackDetected
            ))
        ));
        let other_root = super::super::RootKey::generate(fixture_root().repository()).unwrap();
        let foreign = EncryptedConfig::create(&other_root, b"foreign synthetic password").unwrap();
        let directory = private_tempdir();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = super::super::FileInitializationJournal::create(
            directory.path(),
            session.binding(),
            &foreign,
        )
        .unwrap();
        assert!(matches!(
            unreachable.publish_initial_config(Arc::new(journal)).await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::AuthenticationFailed
            ))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initial_config_persistence_stays_in_lock_boundary_after_caller_abort() {
        use super::super::{
            InitialConfigPublicationOutcome as Outcome, InitializationJournal,
            InitializationRecord, InitializationStage as Stage,
        };
        struct BlockingAdvance {
            journal: Arc<super::super::FileInitializationJournal>,
            entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            released: std::sync::Mutex<bool>,
            wake: std::sync::Condvar,
        }
        impl InitializationJournal for BlockingAdvance {
            fn load(&self, binding: &SessionBinding) -> super::super::Result<InitializationRecord> {
                self.journal.load(binding)
            }
            fn advance(
                &self,
                binding: &SessionBinding,
                expected: Stage,
                next: Stage,
            ) -> super::super::Result<()> {
                self.entered
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                let mut released = self.released.lock().unwrap();
                while !*released {
                    released = self.wake.wait(released).unwrap();
                }
                self.journal.advance(binding, expected, next)
            }
        }
        for abort in [false, true] {
            let session = session();
            let (_directory, journal) = initial_journal(&session);
            let (entered, started) = tokio::sync::oneshot::channel();
            let blocking = Arc::new(BlockingAdvance {
                journal: journal.clone(),
                entered: std::sync::Mutex::new(Some(entered)),
                released: std::sync::Mutex::new(false),
                wake: std::sync::Condvar::new(),
            });
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                Reply {
                    status: 204,
                    headers: "Graft-Protocol: 1\r\n".into(),
                    bytes: vec![],
                },
                reply(config().as_bytes().to_vec()),
            ])
            .await;
            let client = client(&url, session.clone());
            let persistence = blocking.clone();
            let caller =
                tokio::spawn(async move { client.publish_initial_config(persistence).await });
            started.await.unwrap();
            if abort {
                caller.abort();
            }
            let locking_session = session.clone();
            let mut locking = tokio::spawn(async move { locking_session.lock().await });
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut locking)
                    .await
                    .is_err()
            );
            assert_eq!(session.state(), SecretSessionState::Locking);
            *blocking.released.lock().unwrap() = true;
            blocking.wake.notify_all();
            locking.await.unwrap();
            let result = caller.await;
            if abort {
                assert!(result.is_err_and(|error| error.is_cancelled()));
            } else {
                assert!(matches!(
                    result.unwrap().unwrap(),
                    Outcome::OutcomeUnknown {
                        error: Some(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
                    }
                ));
            }
            assert_eq!(session.state(), SecretSessionState::Locked);
            assert_eq!(
                journal.load(session.binding()).unwrap().stage,
                Stage::ConfigPublished
            );
            assert_eq!(served.await.unwrap().len(), 3);
        }
    }

    fn range_reply(wire: &[u8], range: Range<usize>) -> Reply {
        Reply {
            status: 206,
            headers: format!(
                "Graft-Protocol: 1\r\nContent-Range: bytes {}-{}/{}\r\n",
                range.start,
                range.end - 1,
                wire.len()
            ),
            bytes: wire[range].to_vec(),
        }
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            assert!(request.len() < 16 * 1024);
            request.push(stream.read_u8().await.unwrap());
        }
        let headers = std::str::from_utf8(&request).unwrap();
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("Content-Length"))
            .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
        assert!(length <= 65536);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        if headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case("content-type: application/octet-stream"))
        {
            request.extend_from_slice(b"hex:");
            for byte in body {
                request.extend_from_slice(format!("{byte:02x}").as_bytes());
            }
        } else {
            request.extend(body);
        }
        String::from_utf8(request).unwrap()
    }

    fn mutation_fixture(session: &SecretSession) -> PreparedRefMutation {
        let operation = session.begin_operation().unwrap();
        let old = EncryptedRef::seal(
            &fixture_root(),
            "refs/heads/private-name",
            &RefTarget::Commit([7; 32]),
        )
        .unwrap();
        let observation = operation
            .observe_ref("refs/heads/private-name", Some(old.as_bytes()))
            .unwrap();
        PreparedRefMutation::update(
            &operation,
            "refs/heads/private-name",
            &RefTarget::Commit([8; 32]),
            observation,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn native_opaque_discovery_paginates_and_authenticates_listed_refs() {
        let first = EncryptedRef::seal(
            &fixture_root(),
            "refs/heads/private-a",
            &RefTarget::Commit([7; 32]),
        )
        .unwrap();
        let second = EncryptedRef::seal(
            &fixture_root(),
            "refs/heads/private-b",
            &RefTarget::Commit([8; 32]),
        )
        .unwrap();
        let mut refs = [
            ("refs/heads/private-a", first),
            ("refs/heads/private-b", second),
        ];
        refs.sort_by(|a, b| a.1.remote_key().cmp(b.1.remote_key()));
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()), reply(config().as_bytes().to_vec()),
            reply(serde_json::to_vec(&serde_json::json!({"paths":[refs[0].1.remote_key()],"next_cursor":"page+2/=cursor"})).unwrap()),
            reply(serde_json::to_vec(&serde_json::json!({"paths":[refs[1].1.remote_key()]})).unwrap()),
            reply(refs[0].1.as_bytes().to_vec()), reply(refs[1].1.as_bytes().to_vec()),
        ]).await;
        let session = session();
        let client = client(&url, session.clone());
        let keys = client
            .list_opaque(
                super::super::EncryptedListClass::Refs,
                super::super::DiscoveryBudget { max_objects: 2, max_pages: 2 },
            )
            .await
            .unwrap();
        assert_eq!(
            keys,
            refs.iter()
                .map(|(_, r)| r.remote_key().to_owned())
                .collect::<Vec<_>>()
        );
        for (key, (logical, reference)) in keys.iter().zip(&refs) {
            let (discovered, observation) = client.discover_ref(key).await.unwrap().unwrap();
            assert_eq!(&discovered, logical);
            assert_eq!(observation.expected_bytes(), Some(reference.as_bytes()));
            session
                .begin_operation()
                .unwrap()
                .validate_observation(logical, &observation)
                .unwrap();
        }
        let requests = served.await.unwrap();
        assert!(requests[2].starts_with("GET /alice/repo/list?prefix=e2ee%2Frefs%2F&limit=2 "));
        assert!(requests[3].contains("cursor=page%2B2%2F%3Dcursor"));
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("private-a") && !request.contains("private-b"))
        );
    }

    #[tokio::test]
    async fn native_object_publication_authenticates_existing_identity_after_response_loss() {
        for (status, kind) in [(0, 0), (412, 0), (412, 1), (412, 2)] {
            let session = session();
            let directory = private_tempdir();
            let candidate = PreparedEncryptedObject::prepare(
                &session.begin_operation().unwrap(),
                super::super::ObjectSnapshot {
                    logical_key: "store/files/private-upload".into(),
                    plaintext_length: 4,
                    plaintext_sha256: Sha256::digest(b"data").into(),
                    chunk_size: super::super::ChunkSize::KiB16,
                },
                &mut b"data".as_slice(),
                directory.path(),
                10000,
            )
            .await
            .unwrap();
            let bytes = if kind == 1 { b"evil" } else { b"data" };
            let (mut wire, chunks, _) = super::super::tests::encrypted_object(
                &fixture_root(),
                "store/files/private-upload",
                bytes,
                super::super::ChunkSize::KiB16,
            );
            let end = wire.len();
            wire.extend(chunks.into_iter().flatten());
            if kind == 2 {
                wire[end - 1] ^= 1;
            }
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
                Reply {
                    status,
                    headers: "Graft-Protocol: 1\r\n".into(),
                    bytes: Vec::new(),
                },
                range_reply(&wire, 0..72),
                range_reply(&wire, 72..end),
            ])
            .await;
            let result = client(&url, session).publish_object(&candidate).await;
            match kind {
                0 => assert!(matches!(result, Ok(ObjectPublicationOutcome::Reused))),
                1 => assert!(matches!(
                    result,
                    Err(EncryptedHttpError::Encryption(
                        EncryptionError::ImmutableConflict
                    ))
                )),
                _ => assert!(matches!(
                    result,
                    Ok(ObjectPublicationOutcome::OutcomeUnknown {
                        error: Some(EncryptedHttpError::Encryption(
                            EncryptionError::AuthenticationFailed
                        ))
                    })
                )),
            }
            let requests = served.await.unwrap();
            assert_eq!(requests.len(), 5);
            assert!(requests[2].starts_with(&format!(
                "PUT /alice/repo/raw-if-not-exists/{} ",
                candidate.remote_key()
            )));
            assert!(!requests[2].contains("private-upload"));
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn native_object_publication_checks_budget_and_unlock_epoch_before_dispatch() {
        let session = session();
        let directory = private_tempdir();
        let candidate = PreparedEncryptedObject::prepare(
            &session.begin_operation().unwrap(),
            super::super::ObjectSnapshot {
                logical_key: "segments/upload".into(),
                plaintext_length: 0,
                plaintext_sha256: Sha256::digest([]).into(),
                chunk_size: super::super::ChunkSize::KiB64,
            },
            &mut b"".as_slice(),
            directory.path(),
            10000,
        )
        .await
        .unwrap();
        let mut descriptor = descriptor();
        descriptor["limits"] = serde_json::json!({"max_request_bytes": 1});
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor).unwrap()),
            reply(config().as_bytes().to_vec()),
        ])
        .await;
        let client = client(&url, session.clone());
        assert!(matches!(
            client.publish_object(&candidate).await,
            Err(EncryptedHttpError::Encryption(EncryptionError::TooLarge))
        ));
        assert_eq!(served.await.unwrap().len(), 2);
        session.lock().await;
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        assert!(matches!(
            client.publish_object(&candidate).await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::StaleObservation
            ))
        ));
    }

    #[tokio::test]
    async fn native_ref_publication_lock_after_dispatch_preserves_unknown_outcome() {
        let session = session();
        let candidate = mutation_fixture(&session);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/repo", listener.local_addr().unwrap());
        let (started, receiving) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let served = tokio::spawn(async move {
            for response in [
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                write_reply(&mut stream, &response).await;
            }
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(request.starts_with("POST /repo/cas/e2ee/refs/"));
            started.send(()).unwrap();
            // The server has the full candidate but has not acknowledged publication.
            released.await.unwrap();
        });
        let client = client(&url, session.clone());
        let publishing = tokio::spawn(async move { client.publish_ref(&candidate).await });
        tokio::time::timeout(Duration::from_secs(2), receiving)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), session.lock())
            .await
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert!(matches!(
            publishing.await.unwrap().unwrap(),
            RefPublicationOutcome::OutcomeUnknown {
                error: Some(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
            }
        ));
        release.send(()).unwrap();
        served.await.unwrap();
    }

    #[tokio::test]
    async fn native_ref_publication_checks_request_budget_before_dispatch() {
        let session = session();
        let candidate = mutation_fixture(&session);
        let mut descriptor = descriptor();
        descriptor["limits"] = serde_json::json!({"max_request_bytes": 1});
        let (url, task) = server(vec![
            reply(serde_json::to_vec(&descriptor).unwrap()),
            reply(config().as_bytes().to_vec()),
        ])
        .await;
        assert!(matches!(
            client(&url, session).publish_ref(&candidate).await,
            Err(EncryptedHttpError::Encryption(EncryptionError::TooLarge))
        ));
        assert_eq!(task.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn native_ref_publication_reconciles_lost_responses_and_mismatches() {
        for (status, current, outcome) in [
            (0, 0, 0),
            (0, 1, 1),
            (0, 2, 3),
            (409, 0, 0),
            (409, 1, 1),
            (409, 2, 2),
            (412, 0, 0),
            (412, 1, 1),
            (412, 2, 2),
        ] {
            let session = session();
            let candidate = mutation_fixture(&session);
            let different = EncryptedRef::seal(
                &fixture_root(),
                "refs/heads/private-name",
                &RefTarget::Commit([9; 32]),
            )
            .unwrap();
            let bytes = match current {
                0 => candidate.replacement_bytes().unwrap(),
                1 => candidate.expected_bytes().unwrap(),
                _ => different.as_bytes(),
            }
            .to_vec();
            let (url, task) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
                Reply {
                    status,
                    headers: "Graft-Protocol: 1\r\n".into(),
                    bytes: Vec::new(),
                },
                reply(bytes),
            ])
            .await;
            let result = client(&url, session).publish_ref(&candidate).await.unwrap();
            assert!(matches!(
                (&result, outcome),
                (RefPublicationOutcome::Confirmed, 0)
                    | (RefPublicationOutcome::Unconfirmed, 1)
                    | (RefPublicationOutcome::Conflict, 2)
                    | (RefPublicationOutcome::OutcomeUnknown { error: Some(_) }, 3)
            ));
            let requests = task.await.unwrap();
            assert_eq!(requests.len(), 4);
            assert!(
                requests[2]
                    .starts_with(&format!("POST /alice/repo/cas/{} ", candidate.remote_key()))
            );
            let expected = candidate
                .expected_bytes()
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            assert!(requests[2].contains(&format!("x-graft-expected-hex: {expected}\r\n")));
            assert!(!requests[2].contains("private-name"));
        }
    }

    #[tokio::test]
    async fn native_ref_journal_persistence_stays_in_lock_boundary_after_caller_abort() {
        use super::super::{RefMutationJournal, RefMutationRecord};
        struct Blocking {
            entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
            released: std::sync::Mutex<bool>,
            wake: std::sync::Condvar,
        }
        impl RefMutationJournal for Blocking {
            fn load(&self, _: &SessionBinding) -> super::super::Result<Option<RefMutationRecord>> {
                unreachable!()
            }
            fn bind(&self, _: &SessionBinding, _: &RefMutationRecord) -> super::super::Result<()> {
                self.entered
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send(())
                    .unwrap();
                let mut released = self.released.lock().unwrap();
                while !*released {
                    released = self.wake.wait(released).unwrap();
                }
                Ok(())
            }
        }
        for abort in [false, true] {
            let session = session();
            let candidate = mutation_fixture(&session);
            let (entered, started) = tokio::sync::oneshot::channel();
            let journal = Arc::new(Blocking {
                entered: std::sync::Mutex::new(Some(entered)),
                released: std::sync::Mutex::new(false),
                wake: std::sync::Condvar::new(),
            });
            let client = client("http://127.0.0.1:1/alice/repo", session.clone());
            let binding = journal.clone();
            let publishing =
                tokio::spawn(async move { client.publish_ref_durable(&candidate, binding).await });
            started.await.unwrap();
            if abort {
                publishing.abort();
            }
            let locking_session = session.clone();
            let mut locking = tokio::spawn(async move { locking_session.lock().await });
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut locking)
                    .await
                    .is_err()
            );
            assert_eq!(session.state(), SecretSessionState::Locking);
            *journal.released.lock().unwrap() = true;
            journal.wake.notify_all();
            tokio::time::timeout(Duration::from_secs(2), locking)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(session.state(), SecretSessionState::Locked);
            if abort {
                assert!(matches!(publishing.await, Err(error) if error.is_cancelled()));
            } else {
                assert!(matches!(
                    publishing.await.unwrap(),
                    Err(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
                ));
            }
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn native_ref_journal_reopen_preserves_ciphertext_after_response_loss() {
        use super::super::{FileRefMutationJournal, RefMutationJournal};
        use std::{fs, os::unix::fs::PermissionsExt};
        let directory = private_tempdir();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let first_session = session();
        let candidate = mutation_fixture(&first_session);
        let old = candidate.expected_bytes().unwrap().to_vec();
        let new = candidate.replacement_bytes().unwrap().to_vec();
        let journal = Arc::new(
            FileRefMutationJournal::create(directory.path(), first_session.binding()).unwrap(),
        );
        let lost = || Reply {
            status: 0,
            headers: String::new(),
            bytes: Vec::new(),
        };
        let (url, task) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            lost(),
            reply(old.clone()),
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            lost(),
            reply(new.clone()),
        ])
        .await;
        let first = client(&url, first_session.clone());
        assert!(matches!(
            first
                .publish_ref_durable(&candidate, journal.clone())
                .await
                .unwrap(),
            RefPublicationOutcome::Unconfirmed
        ));
        assert!(journal.load(first_session.binding()).unwrap().is_some());
        first_session.disconnect().await;
        drop(first);
        drop(candidate);
        drop(journal);
        let next_session = session();
        let journal = Arc::new(
            FileRefMutationJournal::open(directory.path(), next_session.binding()).unwrap(),
        );
        let next = client(&url, next_session);
        let restored = next
            .restore_ref_mutation("refs/heads/private-name", journal.clone())
            .await
            .unwrap();
        assert_eq!(restored.expected_bytes(), Some(old.as_slice()));
        assert_eq!(restored.replacement_bytes(), Some(new.as_slice()));
        assert!(matches!(
            next.publish_ref_durable(&restored, journal).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 8);
        assert_eq!(
            requests[2].split("\r\n\r\n").nth(1),
            requests[6].split("\r\n\r\n").nth(1)
        );
        let expected = old
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        for request in [&requests[2], &requests[6]] {
            assert!(request.contains(&format!("x-graft-expected-hex: {expected}\r\n")));
            assert!(!request.contains("private-name"));
        }
    }

    #[tokio::test]
    async fn native_ref_publication_retries_identical_candidate_ciphertext() {
        let session = session();
        let candidate = mutation_fixture(&session);
        let lost = || Reply {
            status: 0,
            headers: String::new(),
            bytes: Vec::new(),
        };
        let (url, task) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            lost(),
            reply(candidate.expected_bytes().unwrap().to_vec()),
            lost(),
            reply(candidate.replacement_bytes().unwrap().to_vec()),
        ])
        .await;
        let client = client(&url, session);
        assert!(matches!(
            client.publish_ref(&candidate).await.unwrap(),
            RefPublicationOutcome::Unconfirmed
        ));
        assert!(matches!(
            client.publish_ref(&candidate).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        let requests = task.await.unwrap();
        assert_eq!(requests.len(), 6);
        assert_eq!(requests[2], requests[4]);
    }

    #[tokio::test]
    async fn native_ref_publication_rejects_corrupt_reconciliation_and_stale_candidates() {
        let session = session();
        let candidate = mutation_fixture(&session);
        let mut corrupt = candidate.replacement_bytes().unwrap().to_vec();
        *corrupt.last_mut().unwrap() ^= 1;
        let (url, task) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            Reply {
                status: 412,
                headers: "Graft-Protocol: 1\r\n".into(),
                bytes: Vec::new(),
            },
            reply(corrupt),
        ])
        .await;
        let client = client(&url, session.clone());
        assert!(matches!(
            client.publish_ref(&candidate).await.unwrap(),
            RefPublicationOutcome::OutcomeUnknown {
                error: Some(EncryptedHttpError::Encryption(
                    EncryptionError::AuthenticationFailed
                ))
            }
        ));
        task.await.unwrap();
        session.lock().await;
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        assert!(matches!(
            client.publish_ref(&candidate).await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::StaleObservation
            ))
        ));
    }

    struct TestBundle {
        frames: Vec<(String, Vec<u8>)>,
        plaintext: std::collections::HashMap<String, Vec<u8>>,
    }

    fn bundle_fixture() -> TestBundle {
        let root = fixture_root();
        let mut frames = Vec::new();
        let mut plaintext = std::collections::HashMap::new();
        for (logical, bytes) in [
            ("objects/pack/private.pack", vec![91; 32769]),
            ("objects/pack/private.idx", b"idx".to_vec()),
            ("segments/empty", Vec::new()),
        ] {
            let (mut wire, chunks, _) = super::super::tests::encrypted_object(
                &root,
                logical,
                &bytes,
                super::super::ChunkSize::KiB16,
            );
            wire.extend(chunks.into_iter().flatten());
            frames.push((object::remote_key(&root, logical).unwrap(), wire));
            plaintext.insert(logical.to_owned(), bytes);
        }
        let pack = object::remote_key(&root, "objects/pack/private.pack").unwrap();
        let index = object::remote_key(&root, "objects/pack/private.idx").unwrap();
        let hint = super::super::RoutingHint::create(&root, &pack, &index, &[]).unwrap();
        frames.push((hint.remote_key().to_owned(), hint.as_bytes().to_vec()));
        frames.sort_by(|a, b| a.0.cmp(&b.0));
        TestBundle { frames, plaintext }
    }

    fn bundle_wire(frames: &[(String, Vec<u8>)]) -> Vec<u8> {
        let descriptors: Vec<Vec<u8>> = frames
            .iter()
            .map(|(key, bytes)| {
                super::super::codec::encode(&[key.as_bytes(), &(bytes.len() as u64).to_be_bytes()])
                    .unwrap()
            })
            .collect();
        let descriptors =
            super::super::codec::encode(&descriptors.iter().map(Vec::as_slice).collect::<Vec<_>>())
                .unwrap();
        let manifest =
            super::super::codec::encode(&[&1_u16.to_be_bytes(), &[], &[], &descriptors]).unwrap();
        let mut wire = b"GRAFTEB1".to_vec();
        wire.extend_from_slice(&(manifest.len() as u32).to_be_bytes());
        wire.extend(manifest);
        for (key, bytes) in frames {
            wire.extend_from_slice(&(key.len() as u32).to_be_bytes());
            wire.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            wire.extend_from_slice(key.as_bytes());
            wire.extend_from_slice(bytes);
        }
        wire
    }

    fn bundle_reply(wire: Vec<u8>, count: usize) -> Reply {
        Reply {
            status: 200,
            headers: format!(
                "Graft-Protocol: 1\r\nContent-Type: application/vnd.graft.e2ee-bundle\r\nx-graft-bundle-objects: {count}\r\nx-graft-bundle-total-bytes: {}\r\n",
                wire.len()
            ),
            bytes: wire,
        }
    }

    #[tokio::test]
    async fn native_read_bundle_authenticates_files_and_hints_in_one_request() {
        use std::io::Read;
        let fixture = bundle_fixture();
        let mut keys: Vec<String> = fixture.frames.iter().map(|(key, _)| key.clone()).collect();
        keys.reverse();
        let response = bundle_reply(bundle_wire(&fixture.frames), keys.len());
        let prefix = vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
        ];
        let (url, served) =
            fragmented_download_server(prefix, response, FullBodyFraming::Chunked).await;
        let directory = private_tempdir();
        let entries = client(&url, session())
            .read_bundle(&keys, directory.path(), 32772)
            .await
            .unwrap();
        assert_eq!(entries.len(), keys.len());
        for entry in entries {
            match entry {
                EncryptedBundleEntry::Object { remote_key, value } => {
                    let logical = value.metadata().logical_key.clone();
                    assert_eq!(
                        remote_key,
                        object::remote_key(&fixture_root(), &logical).unwrap()
                    );
                    let mut output = Vec::new();
                    let mut file = value.into_file();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
                    }
                    file.read_to_end(&mut output).unwrap();
                    assert_eq!(&output, fixture.plaintext.get(&logical).unwrap());
                }
                EncryptedBundleEntry::Hint(hint) => {
                    assert_eq!(
                        hint.pack_key(),
                        object::remote_key(&fixture_root(), "objects/pack/private.pack").unwrap()
                    );
                    assert_eq!(
                        hint.index_key(),
                        object::remote_key(&fixture_root(), "objects/pack/private.idx").unwrap()
                    );
                }
            }
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let requests = served.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].starts_with("POST /repo/e2ee/read-bundle HTTP/1.1"));
        assert!(!requests[2].contains("private.pack") && !requests[2].contains("private.idx"));
        let body = requests[2].split_once("\r\n\r\n").unwrap().1;
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            value["keys"],
            serde_json::json!(super::super::bundle::prepare_keys(&keys).unwrap())
        );
        assert!(
            requests[2]
                .to_lowercase()
                .contains(&format!("content-length: {}", body.len()))
        );
    }

    #[tokio::test]
    async fn native_read_bundle_rejects_bad_framing_crypto_and_budget_without_partial_handoff() {
        let fixture = bundle_fixture();
        let keys: Vec<String> = fixture.frames.iter().map(|(key, _)| key.clone()).collect();
        for case in 0..11 {
            let mut response = bundle_reply(bundle_wire(&fixture.frames), keys.len());
            let first_frame =
                12 + u32::from_be_bytes(response.bytes[8..12].try_into().unwrap()) as usize;
            match case {
                0 => response.bytes[0] ^= 1,
                1 => response.bytes[19] ^= 1,
                2 => response.bytes.push(0),
                3 => {
                    let last = response.bytes.len() - 1;
                    response.bytes[last] ^= 1;
                }
                4 => response.bytes[first_frame + 12] ^= 1,
                5 => response.headers = response.headers.replace("objects: 4", "objects: 5"),
                6 => {
                    response.headers = response
                        .headers
                        .replace("application/vnd.graft.e2ee-bundle", "application/json")
                }
                7 => {} // The aggregate disk budget permits the index but rejects the subsequent pack.
                8 => response.bytes[8..12].copy_from_slice(&262145_u32.to_be_bytes()),
                9 => {
                    let mac =
                        first_frame + 12 + fixture.frames[0].0.len() + fixture.frames[0].1.len()
                            - 1;
                    response.bytes[mac] ^= 1;
                }
                _ => response.bytes[first_frame + 11] ^= 1,
            }
            let prefix = vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
            ];
            let (url, served) =
                fragmented_download_server(prefix, response, FullBodyFraming::Chunked).await;
            let directory = private_tempdir();
            let maximum = if case == 7 { 3 } else { 32772 };
            assert!(
                client(&url, session())
                    .read_bundle(&keys, directory.path(), maximum)
                    .await
                    .is_err(),
                "case {case}"
            );
            assert_eq!(served.await.unwrap().len(), 3);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn native_read_bundle_respects_advertised_request_budget_before_post() {
        let fixture = bundle_fixture();
        let keys: Vec<String> = fixture.frames.iter().map(|(key, _)| key.clone()).collect();
        let mut descriptor = descriptor();
        descriptor["limits"] = serde_json::json!({"max_request_bytes":1});
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor).unwrap()),
            reply(config().as_bytes().to_vec()),
        ])
        .await;
        let directory = private_tempdir();
        assert!(matches!(
            client(&url, session())
                .read_bundle(&keys, directory.path(), 32772)
                .await,
            Err(EncryptedHttpError::Encryption(EncryptionError::TooLarge))
        ));
        assert_eq!(served.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn native_read_bundle_lock_and_caller_abort_discard_unpublished_files() {
        for abort in [false, true] {
            let fixture = bundle_fixture();
            let keys: Vec<String> = fixture.frames.iter().map(|(key, _)| key.clone()).collect();
            let response = bundle_reply(bundle_wire(&fixture.frames), keys.len());
            let manifest_end =
                12 + u32::from_be_bytes(response.bytes[8..12].try_into().unwrap()) as usize;
            let prefix_end = manifest_end
                + fixture.frames[..2]
                    .iter()
                    .map(|(key, bytes)| 12 + key.len() + bytes.len())
                    .sum::<usize>()
                + 1;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/repo", listener.local_addr().unwrap());
            let (started, receiving) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let served = tokio::spawn(async move {
                for response in [
                    reply(serde_json::to_vec(&descriptor()).unwrap()),
                    reply(config().as_bytes().to_vec()),
                ] {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    read_request(&mut stream).await;
                    write_reply(&mut stream, &response).await;
                }
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                let headers = format!(
                    "HTTP/1.1 200 Test\r\n{}Content-Length: {}\r\n\r\n",
                    response.headers,
                    response.bytes.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream
                    .write_all(&response.bytes[..prefix_end])
                    .await
                    .unwrap();
                started.send(()).unwrap();
                released.await.unwrap();
            });
            let session = session();
            let client = client(&url, session.clone());
            let directory = private_tempdir();
            let path = directory.path().to_path_buf();
            let reading =
                tokio::spawn(async move { client.read_bundle(&keys, &path, 32772).await });
            tokio::time::timeout(Duration::from_secs(2), receiving)
                .await
                .unwrap()
                .unwrap();
            if abort {
                reading.abort();
                assert!(matches!(reading.await, Err(error) if error.is_cancelled()));
                tokio::time::timeout(Duration::from_secs(2), session.lock())
                    .await
                    .unwrap();
            } else {
                tokio::time::timeout(Duration::from_secs(2), session.lock())
                    .await
                    .unwrap();
                assert!(matches!(
                    reading.await.unwrap(),
                    Err(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
                ));
            }
            assert_eq!(session.state(), SecretSessionState::Locked);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
            release.send(()).unwrap();
            served.await.unwrap();
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    #[ignore = "helper invoked by the actual-service tag recovery test"]
    async fn native_tag_journal_recovery_child() {
        use super::super::{FileRefMutationJournalProvider, RefMutationJournalProvider};
        use std::os::unix::fs::PermissionsExt;
        let Some(directory) = std::env::var_os("GRAFT_TEST_TAG_JOURNAL") else {
            return;
        };
        let url = std::env::var("GRAFT_TEST_TAG_URL").unwrap();
        let key = std::env::var("GRAFT_TEST_TAG_KEY").unwrap();
        let digest = std::env::var("GRAFT_TEST_TAG_CANDIDATE_DIGEST").unwrap();
        let session = locked_session();
        client(&url, session.clone())
            .unlock(zeroize::Zeroizing::new(b"http sync password".to_vec()))
            .await
            .unwrap();
        let provider = Arc::new(
            FileRefMutationJournalProvider::open(Path::new(&directory), session.binding()).unwrap(),
        );
        let slot = provider.journal(session.binding(), &key).unwrap();
        let record = slot.load(session.binding()).unwrap().unwrap();
        assert!(record.expected.is_none());
        assert_eq!(
            format!("{:x}", Sha256::digest(record.replacement.as_ref().unwrap())),
            digest
        );
        let staging = private_tempdir();
        std::fs::set_permissions(staging.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = crate::remote::RemoteConfig::EncryptedHttp {
            url,
            uuid: descriptor()["encryption"]["uuid"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let credentials = RemoteCredentials::explicit();
        credentials
            .set_http_bearer_token("origin", "account-token".into())
            .unwrap();
        credentials
            .set_encrypted_session_with_journals(
                "origin",
                &config,
                session.clone(),
                crate::remote::EncryptedTransferBudget {
                    private_directory: staging.path().to_path_buf(),
                    max_buffered_plaintext: 1024 * 1024,
                    max_ciphertext_spool: 2 * 1024 * 1024,
                },
                provider,
            )
            .unwrap();
        let remote =
            crate::remote::Remote::with_config_and_credentials(config, "origin", &credentials)
                .unwrap();
        let path = "refs/tags/durable-private-release";
        let observed = remote.observe_ref(path).await.unwrap();
        let bytes = remote
            .read_observed_tag_object(path, &observed)
            .await
            .unwrap()
            .unwrap();
        let (_, retained) =
            EncryptedRef::open(&fixture_root(), path, record.replacement.as_ref().unwrap())
                .unwrap();
        assert!(retained == RefTarget::from_annotated_tag(&bytes).unwrap());
        remote
            .publish_observed_tag_ref(path, &observed, bytes)
            .await
            .unwrap();
        assert!(slot.load(session.binding()).unwrap().is_none());
        session.disconnect().await;
    }

    #[tokio::test]
    #[ignore = "requires Node.js and pnpm --filter @eidos.space/graft-remote build"]
    async fn native_client_interoperates_with_actual_typescript_encrypted_service() {
        use std::io::Read;
        use std::process::Stdio;
        use tokio::io::AsyncBufReadExt;
        let fixture = bundle_fixture();
        let keys: Vec<String> = fixture.frames.iter().map(|(key, _)| key.clone()).collect();
        let sealed = EncryptedRef::seal(
            &fixture_root(),
            "refs/heads/private-name",
            &RefTarget::Commit([7; 32]),
        )
        .unwrap();
        let mut objects = fixture.frames.clone();
        objects.push(("e2ee/config".to_owned(), config().as_bytes().to_vec()));
        objects.push((sealed.remote_key().to_owned(), sealed.as_bytes().to_vec()));
        let directory = private_tempdir();
        let fixture_path = directory.path().join("encrypted-fixture.json");
        let capture_path = directory.path().join("requests.json");
        let migration_id = super::super::RepositoryId::generate().unwrap();
        let migration_uuid = migration_id
            .as_bytes()
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                format!(
                    "{}{byte:02x}",
                    if [4, 6, 8, 10].contains(&index) {
                        "-"
                    } else {
                        ""
                    }
                )
            })
            .collect::<String>();
        let mut migration_encryption = descriptor()["encryption"].clone();
        migration_encryption["uuid"] = migration_uuid.into();
        let value = serde_json::json!({"encryption":descriptor()["encryption"],"objects":objects,
            "dropInitialConfigResponse":true,"dropConfigMutationAcknowledgements":true,
            "migrationEncryption":migration_encryption,"dropMigrationConfigAcknowledgements":true,
            "dropRefMutationAcknowledgements":[EncryptedRef::seal(
                &fixture_root(), "refs/tags/durable-private-release", &RefTarget::Commit([7; 32])
            ).unwrap().remote_key()]});
        std::fs::write(&fixture_path, serde_json::to_vec(&value).unwrap()).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut child = tokio::process::Command::new("node")
            .arg(root.join("tests/e2ee/remote-service.mjs"))
            .arg(&fixture_path)
            .arg(&capture_path)
            .current_dir(&root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let ready = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("TypeScript harness exited before readiness");
        let ready: serde_json::Value = serde_json::from_str(&ready).unwrap();
        #[cfg(unix)]
        {
            use super::super::{
                FileCheckpointStore, FileInitializationJournal,
                InitialConfigPublicationOutcome as Outcome, InitializationJournal,
                InitializationStage,
            };
            use std::os::unix::fs::PermissionsExt;
            let (checkpoint_directory, initial_session) = persistent_session();
            let root =
                super::super::RootKey::generate(initial_session.binding().repository()).unwrap();
            let candidate =
                EncryptedConfig::create(&root, b"initial enrollment synthetic password").unwrap();
            let journal_directory = private_tempdir();
            std::fs::set_permissions(
                journal_directory.path(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            let journal = Arc::new(
                FileInitializationJournal::create(
                    journal_directory.path(),
                    initial_session.binding(),
                    &candidate,
                )
                .unwrap(),
            );
            initial_session.unlock_with_root(&candidate, root).unwrap();
            let initial_client = client(
                ready["enrollmentUrl"].as_str().unwrap(),
                initial_session.clone(),
            );
            assert!(matches!(
                initial_client
                    .publish_initial_config(journal.clone())
                    .await
                    .unwrap(),
                Outcome::Confirmed
            ));
            assert_eq!(
                journal.load(initial_session.binding()).unwrap().stage,
                InitializationStage::ConfigPublished
            );
            initial_session.lock().await;
            drop(initial_client);
            drop(initial_session);
            drop(journal);

            // Independent session, reopened checkpoint/journal, same config and
            // password; neither the old root nor the old client cache survives.
            let binding = SessionBinding::new(
                "native.test".into(),
                "test-account".into(),
                candidate.repository(),
            )
            .unwrap();
            let store = FileCheckpointStore::open(checkpoint_directory.path(), &binding).unwrap();
            let journal = Arc::new(
                FileInitializationJournal::open(journal_directory.path(), &binding).unwrap(),
            );
            let record = journal.load(&binding).unwrap();
            assert_eq!(record.config.as_bytes(), candidate.as_bytes());
            let resumed = SecretSession::with_checkpoint_store(binding, Arc::new(store)).unwrap();
            resumed
                .unlock(
                    record.config,
                    zeroize::Zeroizing::new(b"initial enrollment synthetic password".to_vec()),
                )
                .await
                .unwrap();
            let resumed_client = client(ready["enrollmentUrl"].as_str().unwrap(), resumed.clone());
            assert!(matches!(
                resumed_client
                    .reconcile_initial_config(journal.clone())
                    .await
                    .unwrap(),
                Outcome::Confirmed
            ));
            resumed_client.verify().await.unwrap();
            assert_eq!(
                journal.load(resumed.binding()).unwrap().stage,
                InitializationStage::ConfigPublished
            );
            resumed.lock().await;
            let captured: Vec<serde_json::Value> =
                serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
            let mutations: Vec<_> = captured
                .iter()
                .filter(|request| {
                    request["method"] == "PUT"
                        && request["path"] == "/acme/enrollment/raw-if-not-exists/e2ee/config"
                })
                .collect();
            assert_eq!(mutations.len(), 1);
            assert_eq!(mutations[0]["droppedInitialConfigResponse"], true);
            assert_eq!(
                mutations[0]["body"],
                candidate
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            assert!(
                !serde_json::to_string(&captured)
                    .unwrap()
                    .contains("initial enrollment synthetic password")
            );
        }
        #[cfg(unix)]
        let (_source_checkpoint_directory, session) = persistent_session();
        #[cfg(not(unix))]
        let session = locked_session();
        let client = client(ready["url"].as_str().unwrap(), session.clone());
        client
            .unlock(zeroize::Zeroizing::new(b"http sync password".to_vec()))
            .await
            .unwrap();
        let entries = client
            .read_bundle(&keys, directory.path(), 32772)
            .await
            .unwrap();
        for entry in entries {
            if let EncryptedBundleEntry::Object { value, .. } = entry {
                let logical = value.metadata().logical_key.clone();
                let mut bytes = Vec::new();
                value.into_file().read_to_end(&mut bytes).unwrap();
                assert_eq!(&bytes, fixture.plaintext.get(&logical).unwrap());
            }
        }
        assert_eq!(
            client
                .read_object_range("objects/pack/private.pack", 16382..16386)
                .await
                .unwrap()
                .unwrap(),
            vec![91; 4]
        );
        let observation = client.read_ref("refs/heads/private-name").await.unwrap();
        assert_eq!(observation.expected_bytes(), Some(sealed.as_bytes()));
        let candidate = client
            .prepare_ref_update(
                "refs/heads/private-name",
                &RefTarget::Commit([8; 32]),
                observation,
            )
            .await
            .unwrap();
        assert!(matches!(
            client.publish_ref(&candidate).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        let updated = client.read_ref("refs/heads/private-name").await.unwrap();
        assert_eq!(updated.expected_bytes(), candidate.replacement_bytes());
        // Replaying the same candidate receives 412, then confirms its exact ciphertext.
        assert!(matches!(
            client.publish_ref(&candidate).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        // Typed tag transport retains its authoritative object and commit edge.
        // Content verification itself is exercised by the repository-byte tests.
        let tag_path = "refs/tags/private-annotated-tag";
        let tag_target = RefTarget::AnnotatedTag { object: [19; 32], commit: [8; 32] };
        let tag_absent = client.read_ref(tag_path).await.unwrap();
        let tag_candidate = client
            .prepare_ref_update(tag_path, &tag_target, tag_absent)
            .await
            .unwrap();
        assert!(matches!(
            client.publish_ref(&tag_candidate).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        let tag_observed = client.read_ref(tag_path).await.unwrap();
        assert!(tag_observed.target() == Some(&tag_target));
        assert_eq!(
            tag_observed.expected_bytes(),
            tag_candidate.replacement_bytes()
        );
        assert!(matches!(
            client.publish_ref(&tag_candidate).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        let tag_delete = client
            .prepare_ref_delete(tag_path, tag_observed)
            .await
            .unwrap();
        assert!(matches!(
            client.publish_ref(&tag_delete).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        assert!(client.read_ref(tag_path).await.unwrap().target().is_none());
        let absent = client.read_ref("HEAD").await.unwrap();
        let head = client
            .prepare_ref_update(
                "HEAD",
                &RefTarget::Symbolic("refs/heads/private-name".into()),
                absent,
            )
            .await
            .unwrap();
        #[cfg(unix)]
        let head_journal_directory = private_tempdir();
        #[cfg(unix)]
        let head_journal = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                head_journal_directory.path(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            Arc::new(
                super::super::FileRefMutationJournal::create(
                    head_journal_directory.path(),
                    session.binding(),
                )
                .unwrap(),
            )
        };
        #[cfg(unix)]
        assert!(matches!(
            client
                .publish_ref_durable(&head, head_journal.clone())
                .await
                .unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        #[cfg(not(unix))]
        assert!(matches!(
            client.publish_ref(&head).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        let observed_head = client.read_ref("HEAD").await.unwrap();
        assert_eq!(observed_head.expected_bytes(), head.replacement_bytes());
        let listed_refs = client
            .list_opaque(
                super::super::EncryptedListClass::Refs,
                super::super::DiscoveryBudget::default(),
            )
            .await
            .unwrap();
        assert_eq!(listed_refs, vec![sealed.remote_key()]);
        let (logical, discovered) = client.discover_ref(&listed_refs[0]).await.unwrap().unwrap();
        assert_eq!(logical, "refs/heads/private-name");
        assert_eq!(discovered.expected_bytes(), candidate.replacement_bytes());
        let indexes = client
            .list_opaque(
                super::super::EncryptedListClass::Indexes,
                super::super::DiscoveryBudget::default(),
            )
            .await
            .unwrap();
        assert_eq!(indexes.len(), 1);
        let metadata = client.discover_object(&indexes[0]).await.unwrap().unwrap();
        assert_eq!(metadata.logical_key, "objects/pack/private.idx");
        let deletion = client
            .prepare_ref_delete("refs/heads/private-name", updated)
            .await
            .unwrap();
        assert!(matches!(
            client.publish_ref(&deletion).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        assert!(
            client
                .read_ref("refs/heads/private-name")
                .await
                .unwrap()
                .expected_bytes()
                .is_none()
        );
        let downloaded = client
            .download_object("objects/pack/private.idx", directory.path(), 3)
            .await
            .unwrap()
            .unwrap();
        let mut bytes = Vec::new();
        downloaded.into_file().read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"idx");
        let plaintext = vec![41; 200003];
        let prepared = PreparedEncryptedObject::prepare(
            &session.begin_operation().unwrap(),
            super::super::ObjectSnapshot {
                logical_key: "store/files/private-upload".into(),
                plaintext_length: plaintext.len() as u64,
                plaintext_sha256: Sha256::digest(&plaintext).into(),
                chunk_size: super::super::ChunkSize::KiB64,
            },
            &mut plaintext.as_slice(),
            directory.path(),
            300000,
        )
        .await
        .unwrap();
        assert!(matches!(
            client.publish_object(&prepared).await.unwrap(),
            ObjectPublicationOutcome::Created
        ));
        assert!(matches!(
            client.publish_object(&prepared).await.unwrap(),
            ObjectPublicationOutcome::Reused
        ));
        let downloaded = client
            .download_object("store/files/private-upload", directory.path(), 200003)
            .await
            .unwrap()
            .unwrap();
        let mut returned = Vec::new();
        downloaded.into_file().read_to_end(&mut returned).unwrap();
        assert_eq!(returned, plaintext);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let adapter_journal_directory = private_tempdir();
        let adapter_journals = Arc::new(
            super::super::FileRefMutationJournalProvider::create(
                adapter_journal_directory.path(),
                client.session.binding(),
            )
            .unwrap(),
        );
        let adapter = crate::remote::Remote::with_encrypted_http_recovery(
            client.clone(),
            crate::remote::EncryptedTransferBudget {
                private_directory: directory.path().to_path_buf(),
                max_buffered_plaintext: 1024 * 1024,
                max_ciphertext_spool: 2 * 1024 * 1024,
            },
            Some(adapter_journals),
            None,
        )
        .unwrap();
        assert_eq!(
            adapter
                .get_raw("store/files/private-upload")
                .await
                .unwrap()
                .unwrap(),
            plaintext
        );
        assert_eq!(
            adapter
                .get_raw_range("store/files/private-upload", 65534..65540)
                .await
                .unwrap(),
            &plaintext[65534..65540]
        );
        assert!(
            adapter
                .put_raw("refs/heads/adapter-private", "bad")
                .await
                .is_err()
        );
        assert!(
            adapter
                .compare_and_swap_raw("refs/heads/adapter-private", None, "bad")
                .await
                .is_err()
        );
        let repo_dir = private_tempdir();
        let repo = crate::repo::Repository::init(repo_dir.path()).unwrap();
        repo.mark_dirty_path(repo_dir.path().join("app.db"))
            .unwrap();
        let committed = repo.commit("adapter private commit message").unwrap();
        let raw_commit = repo
            .object_store()
            .read_raw(&crate::repo::object::ObjectId::new(committed.id.clone()).unwrap())
            .unwrap()
            .unwrap();
        let crate::repo::object::Object::Commit(commit) =
            crate::repo::object::Object::decode(&raw_commit).unwrap()
        else {
            panic!("expected commit object");
        };
        let ids = [
            crate::repo::object::ObjectId::new(committed.id.clone()).unwrap(),
            commit.tree,
        ];
        let mut pack_bytes = b"graft-object-pack-v1\n".to_vec();
        let mut entries = Vec::new();
        for id in ids {
            let raw = repo.object_store().read_raw(&id).unwrap().unwrap();
            entries.push(serde_json::json!({"id":id,"offset":pack_bytes.len(),"len":raw.len()}));
            pack_bytes.extend_from_slice(&raw);
        }
        let pack_id = blake3::hash(&pack_bytes).to_hex().to_string();
        let pack_path = format!("objects/pack/{pack_id}.pack");
        let index = serde_json::to_vec(&serde_json::json!({
            "version":1,"pack":pack_path,"objects":entries,
            "commits":[{"id":committed.id,"parents":commit.parents}],
        }))
        .unwrap();
        let pack = crate::remote::RemoteObjectPack::new(
            pack_id.clone(),
            pack_bytes.clone().into(),
            index.into(),
        );
        let observed = adapter
            .observe_ref("refs/heads/adapter-private")
            .await
            .unwrap();
        let invalid_pack = crate::remote::RemoteObjectPack::new(
            pack_id.clone(),
            b"invalid pack".as_slice().into(),
            b"{}".as_slice().into(),
        );
        let unpublished = crate::remote::RemoteBundleObject::new(
            "store/files/adapter-unpublished".into(),
            [bytes::Bytes::from_static(
                b"adapter private unpublished marker",
            )],
            true,
        )
        .unwrap();
        assert!(
            adapter
                .publish_observed_object_bundle_and_ref(
                    vec![unpublished],
                    Some(invalid_pack),
                    "refs/heads/adapter-private",
                    &observed,
                    format!("{}\n", committed.id),
                )
                .await
                .is_err()
        );
        assert!(
            !adapter
                .has_raw("store/files/adapter-unpublished")
                .await
                .unwrap()
        );
        adapter
            .publish_observed_object_bundle_and_ref(
                vec![],
                Some(pack),
                "refs/heads/adapter-private",
                &observed,
                format!("{}\n", committed.id),
            )
            .await
            .unwrap();
        assert_eq!(
            adapter.get_raw(&pack_path).await.unwrap().unwrap(),
            pack_bytes
        );
        let crate::remote::ReadBundleOutcome::Downloaded(batch) = adapter
            .get_raw_bundle(&[pack_path.clone(), "store/files/private-upload".into()])
            .await
            .unwrap()
        else {
            panic!("encrypted read-bundle is required");
        };
        assert_eq!(batch.get(&pack_path).unwrap().as_ref(), pack_bytes);
        assert_eq!(
            batch.get("store/files/private-upload").unwrap().as_ref(),
            plaintext
        );
        assert_eq!(
            adapter.list_raw("refs/heads/").await.unwrap(),
            vec!["refs/heads/adapter-private"]
        );
        let published = adapter
            .observe_ref("refs/heads/adapter-private")
            .await
            .unwrap();
        assert_eq!(
            published.bytes().unwrap().as_ref(),
            format!("{}\n", committed.id).as_bytes()
        );
        adapter
            .compare_and_delete_observed_ref("refs/heads/adapter-private", &published)
            .await
            .unwrap();
        assert!(
            adapter
                .observe_ref("refs/heads/adapter-private")
                .await
                .unwrap()
                .bytes()
                .is_none()
        );
        let integration_url = ready["integrationUrl"].as_str().unwrap().to_owned();
        // Publish actual tag bytes through the repository remote adapter, then
        // independently read/verify the authoritative object and commit edge.
        let annotated = repo
            .tag_create_annotated(
                "adapter-release",
                Some(&committed.id),
                "adapter private annotation",
            )
            .unwrap();
        let tag_bytes = repo
            .object_store()
            .read_raw(&crate::repo::object::ObjectId::new(&annotated.object).unwrap())
            .unwrap()
            .unwrap();
        #[cfg(unix)]
        let (streamed_file_id, streamed_object_ids) = {
            let file_remote = crate::remote::Remote::with_encrypted_http(
                client.clone(),
                crate::remote::EncryptedTransferBudget {
                    private_directory: directory.path().to_path_buf(),
                    max_buffered_plaintext: 64 * 1024,
                    max_ciphertext_spool: 2 * 1024 * 1024,
                },
            )
            .unwrap();
            let capture = capture_path.clone();
            let (source_directory, id, content, expected_objects, pack_paths) =
                tokio::task::spawn_blocking(move || {
                    let source_directory = private_tempdir();
                    let source = crate::repo::Repository::init(source_directory.path()).unwrap();
                    let path = source_directory.path().join("migration-private-large.bin");
                    let mut content = vec![57; 1048609];
                    let marker = b"migration streamed private content";
                    content[..marker.len()].copy_from_slice(marker);
                    std::fs::write(&path, &content).unwrap();
                    source.stage_artifact_path(&path).unwrap();
                    source
                        .commit_staged("migration stream source message")
                        .unwrap();
                    source
                        .tag_create_annotated(
                            "migration-packed-tag",
                            None,
                            "migration packed tag message",
                        )
                        .unwrap();
                    let plan = source
                        .plan_migration_source(
                            crate::repo::MigrationRefSelection::All,
                            crate::repo::MigrationPlanningBudget::default(),
                        )
                        .unwrap();
                    assert_eq!(plan.external_payloads().len(), 1);
                    let expected_objects = plan
                        .objects()
                        .iter()
                        .map(|id| {
                            let text = id.as_str();
                            (
                                text.to_owned(),
                                std::fs::read(
                                    source_directory
                                        .path()
                                        .join(".graft/objects")
                                        .join(&text[..2])
                                        .join(&text[2..]),
                                )
                                .unwrap(),
                            )
                        })
                        .collect::<std::collections::BTreeMap<_, _>>();
                    let before = std::fs::read(&capture).unwrap();
                    assert!(
                        source
                            .publish_migration_repository_objects(&plan, &file_remote, 1)
                            .is_err()
                    );
                    assert_eq!(std::fs::read(&capture).unwrap(), before);
                    let (object_id, original_object) = expected_objects.first_key_value().unwrap();
                    let object_path = source_directory
                        .path()
                        .join(".graft/objects")
                        .join(&object_id[..2])
                        .join(&object_id[2..]);
                    let mut corrupt_object = original_object.clone();
                    corrupt_object[0] ^= 1;
                    std::fs::write(&object_path, corrupt_object).unwrap();
                    assert!(
                        source
                            .publish_migration_repository_objects(&plan, &file_remote, 512)
                            .is_err()
                    );
                    assert_eq!(std::fs::read(&capture).unwrap(), before);
                    std::fs::write(&object_path, original_object).unwrap();
                    let before_packs = tokio::runtime::Handle::current()
                        .block_on(file_remote.list_raw("objects/pack/"))
                        .unwrap();
                    for _ in 0..2 {
                        assert_eq!(
                            source
                                .publish_migration_repository_objects(&plan, &file_remote, 512)
                                .unwrap(),
                            expected_objects.len()
                        );
                    }
                    let pack_paths = tokio::runtime::Handle::current()
                        .block_on(file_remote.list_raw("objects/pack/"))
                        .unwrap()
                        .into_iter()
                        .filter(|path| path.ends_with(".idx") && !before_packs.contains(path))
                        .collect::<Vec<_>>();
                    assert!(pack_paths.len() > 1);
                    let id = plan
                        .external_payloads()
                        .keys()
                        .next()
                        .unwrap()
                        .as_str()
                        .to_owned();
                    let before = std::fs::read(&capture).unwrap();
                    assert!(
                        source
                            .publish_migration_file_dependencies(&plan, &file_remote, 1024)
                            .is_err()
                    );
                    assert_eq!(std::fs::read(&capture).unwrap(), before);
                    for _ in 0..2 {
                        assert_eq!(
                            source
                                .publish_migration_file_dependencies(
                                    &plan,
                                    &file_remote,
                                    2 * 1024 * 1024
                                )
                                .unwrap(),
                            content.len() as u64
                        );
                    }
                    // Corrupt the retained immutable source payload without changing refs.
                    let raw = source_directory
                        .path()
                        .join(".graft/store/files")
                        .join(&id[..2])
                        .join(&id[2..]);
                    let original = std::fs::read(&raw).unwrap();
                    let before_wrong_digest = std::fs::read(&capture).unwrap();
                    assert!(matches!(
                        tokio::runtime::Handle::current().block_on(
                            file_remote.publish_verified_file(
                                &format!("store/files/{}/{}", &id[..2], &id[2..]),
                                &raw,
                                original.len() as u64,
                                [0; 32]
                            )
                        ),
                        Err(crate::remote::RemoteErr::Encryption(
                            EncryptedHttpError::Encryption(EncryptionError::SnapshotMismatch)
                        ))
                    ));
                    assert_eq!(std::fs::read(&capture).unwrap(), before_wrong_digest);
                    let mut corrupt = original.clone();
                    corrupt[0] ^= 1;
                    std::fs::write(&raw, corrupt).unwrap();
                    let before = std::fs::read(&capture).unwrap();
                    assert!(
                        source
                            .publish_migration_file_dependencies(
                                &plan,
                                &file_remote,
                                2 * 1024 * 1024
                            )
                            .is_err()
                    );
                    assert_eq!(std::fs::read(&capture).unwrap(), before);
                    std::fs::write(&raw, original).unwrap();
                    (source_directory, id, content, expected_objects, pack_paths)
                })
                .await
                .unwrap();
            let reader = self::client(client.base.as_str(), self::session());
            let mut restored_objects = std::collections::BTreeMap::new();
            for index_path in pack_paths {
                let downloaded = reader
                    .download_object(&index_path, directory.path(), 2 * 1024 * 1024)
                    .await
                    .unwrap()
                    .unwrap();
                let mut index = Vec::new();
                downloaded.into_file().read_to_end(&mut index).unwrap();
                let index: serde_json::Value = serde_json::from_slice(&index).unwrap();
                let pack_path = index["pack"].as_str().unwrap();
                let downloaded = reader
                    .download_object(pack_path, directory.path(), 2 * 1024 * 1024)
                    .await
                    .unwrap()
                    .unwrap();
                let mut pack = Vec::new();
                downloaded.into_file().read_to_end(&mut pack).unwrap();
                assert!(pack.len() <= 512);
                assert!(pack_path.contains(blake3::hash(&pack).to_hex().as_str()));
                for entry in index["objects"].as_array().unwrap() {
                    let offset = usize::try_from(entry["offset"].as_u64().unwrap()).unwrap();
                    let length = usize::try_from(entry["len"].as_u64().unwrap()).unwrap();
                    let bytes = pack[offset..offset + length].to_vec();
                    let id = entry["id"].as_str().unwrap().to_owned();
                    assert_eq!(blake3::hash(&bytes).to_hex().as_str(), id);
                    assert!(restored_objects.insert(id, bytes).is_none());
                }
            }
            assert_eq!(restored_objects, expected_objects);
            let logical = format!("store/files/{}/{}", &id[..2], &id[2..]);
            let downloaded = self::client(client.base.as_str(), self::session())
                .download_object(&logical, directory.path(), 2 * 1024 * 1024)
                .await
                .unwrap()
                .unwrap();
            let mut bytes = Vec::new();
            downloaded.into_file().read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, content);
            drop(source_directory);
            (id, expected_objects.into_keys().collect::<Vec<_>>())
        };
        #[cfg(not(unix))]
        let (streamed_file_id, streamed_object_ids) = (
            "unused-non-unix-streaming-fixture-id".to_owned(),
            Vec::<String>::new(),
        );
        let tag_path = "refs/tags/adapter-private-release";
        let tag_absent = adapter.observe_ref(tag_path).await.unwrap();
        assert!(
            adapter
                .read_observed_tag_object(tag_path, &tag_absent)
                .await
                .unwrap()
                .is_none()
        );
        let before_invalid = std::fs::read(&capture_path).unwrap();
        assert!(
            adapter
                .publish_observed_tag_ref(
                    "refs/tags/wrong-path",
                    &tag_absent,
                    tag_bytes.clone().into()
                )
                .await
                .is_err()
        );
        assert!(
            adapter
                .publish_observed_tag_ref(
                    tag_path,
                    &tag_absent,
                    bytes::Bytes::from_static(b"invalid tag object")
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&capture_path).unwrap(), before_invalid);
        adapter
            .publish_observed_tag_ref(tag_path, &tag_absent, tag_bytes.clone().into())
            .await
            .unwrap();
        let tag_observed = adapter.observe_ref(tag_path).await.unwrap();
        let exact_tag_envelope = client
            .read_ref(tag_path)
            .await
            .unwrap()
            .expected_bytes()
            .unwrap()
            .to_vec();
        assert!(
            adapter
                .publish_observed_tag_ref(tag_path, &tag_absent, tag_bytes.clone().into())
                .await
                .is_err()
        );
        assert_eq!(
            client.read_ref(tag_path).await.unwrap().expected_bytes(),
            Some(exact_tag_envelope.as_slice())
        );
        assert_eq!(
            tag_observed.bytes().unwrap().as_ref(),
            format!("{}\n", annotated.object).as_bytes()
        );
        assert_eq!(
            adapter
                .read_observed_tag_object(tag_path, &tag_observed)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            tag_bytes
        );
        let independent = crate::remote::Remote::with_encrypted_http(
            self::client(client.base.as_str(), self::session()),
            crate::remote::EncryptedTransferBudget {
                private_directory: directory.path().to_path_buf(),
                max_buffered_plaintext: 1024 * 1024,
                max_ciphertext_spool: 2 * 1024 * 1024,
            },
        )
        .unwrap();
        let independent_tag = independent.observe_ref(tag_path).await.unwrap();
        assert_eq!(
            independent
                .read_observed_tag_object(tag_path, &independent_tag)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            tag_bytes
        );
        let before_foreign = std::fs::read(&capture_path).unwrap();
        assert!(
            independent
                .publish_observed_tag_ref(tag_path, &tag_observed, tag_bytes.clone().into())
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&capture_path).unwrap(), before_foreign);
        let light_path = "refs/tags/adapter-private-light";
        let light_absent = adapter.observe_ref(light_path).await.unwrap();
        adapter
            .publish_observed_tag_ref(light_path, &light_absent, raw_commit.clone().into())
            .await
            .unwrap();
        let light_observed = adapter.observe_ref(light_path).await.unwrap();
        assert_eq!(
            light_observed.bytes().unwrap().as_ref(),
            format!("{}\n", committed.id).as_bytes()
        );
        assert_eq!(
            adapter
                .read_observed_tag_object(light_path, &light_observed)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            raw_commit
        );
        // A key holder can encrypt bytes under a false logical object ID. AEAD
        // alone is insufficient: the tag reader must recheck content identity.
        let forged_tag_path = "refs/tags/adapter-false-content";
        let forged_object = crate::repo::object::ObjectId::new("1f".repeat(32)).unwrap();
        let forged_object_path =
            crate::repo::object::LooseObjectStore::relative_path(&forged_object);
        let forged_blob = PreparedEncryptedObject::prepare(
            &session.begin_operation().unwrap(),
            super::super::ObjectSnapshot {
                logical_key: forged_object_path,
                plaintext_length: tag_bytes.len() as u64,
                plaintext_sha256: Sha256::digest(&tag_bytes).into(),
                chunk_size: super::super::ChunkSize::KiB64,
            },
            &mut tag_bytes.as_slice(),
            directory.path(),
            300000,
        )
        .await
        .unwrap();
        assert!(matches!(
            client.publish_object(&forged_blob).await.unwrap(),
            ObjectPublicationOutcome::Created
        ));
        let forged_absent = client.read_ref(forged_tag_path).await.unwrap();
        let forged = client
            .prepare_ref_update(
                forged_tag_path,
                &RefTarget::AnnotatedTag {
                    object: [31; 32],
                    commit: *blake3::Hash::from_hex(&committed.id).unwrap().as_bytes(),
                },
                forged_absent,
            )
            .await
            .unwrap();
        assert!(matches!(
            client.publish_ref(&forged).await.unwrap(),
            RefPublicationOutcome::Confirmed
        ));
        let forged_observed = adapter.observe_ref(forged_tag_path).await.unwrap();
        assert!(
            adapter
                .read_observed_tag_object(forged_tag_path, &forged_observed)
                .await
                .is_err()
        );
        for (path, observed) in [
            (tag_path, &tag_observed),
            (light_path, &light_observed),
            (forged_tag_path, &forged_observed),
        ] {
            adapter
                .compare_and_delete_observed_ref(path, observed)
                .await
                .unwrap();
        }
        #[cfg(unix)]
        {
            use super::super::{FileRefMutationJournalProvider, RefMutationJournalProvider};
            use std::os::unix::fs::PermissionsExt;
            let journal_directory = private_tempdir();
            std::fs::set_permissions(
                journal_directory.path(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            let durable_path = "refs/tags/durable-private-release";
            let durable_config = crate::remote::RemoteConfig::EncryptedHttp {
                url: client.base.to_string(),
                uuid: descriptor()["encryption"]["uuid"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            };
            let durable_budget = crate::remote::EncryptedTransferBudget {
                private_directory: directory.path().to_path_buf(),
                max_buffered_plaintext: 1024 * 1024,
                max_ciphertext_spool: 2 * 1024 * 1024,
            };
            let first_session = self::session();
            let provider = Arc::new(
                FileRefMutationJournalProvider::create(
                    journal_directory.path(),
                    first_session.binding(),
                )
                .unwrap(),
            );
            let credentials = RemoteCredentials::explicit();
            credentials
                .set_http_bearer_token("origin", "account-token".into())
                .unwrap();
            credentials
                .set_encrypted_session_with_journals(
                    "origin",
                    &durable_config,
                    first_session.clone(),
                    durable_budget.clone(),
                    provider.clone(),
                )
                .unwrap();
            let durable = crate::remote::Remote::with_config_and_credentials(
                durable_config.clone(),
                "origin",
                &credentials,
            )
            .unwrap();
            let absent = durable.observe_ref(durable_path).await.unwrap();
            assert!(matches!(
                durable
                    .publish_observed_tag_ref(durable_path, &absent, tag_bytes.clone().into())
                    .await,
                Err(crate::remote::RemoteErr::EncryptedOutcomeUnknown { .. })
            ));
            let key =
                EncryptedRef::seal(&fixture_root(), durable_path, &RefTarget::Commit([7; 32]))
                    .unwrap()
                    .remote_key()
                    .to_owned();
            let record = provider
                .journal(first_session.binding(), &key)
                .unwrap()
                .load(first_session.binding())
                .unwrap()
                .unwrap();
            assert!(record.expected.is_none());
            let exact_candidate = record.replacement.clone().unwrap();
            let (_, target) =
                EncryptedRef::open(&fixture_root(), durable_path, &exact_candidate).unwrap();
            assert!(target == RefTarget::from_annotated_tag(&tag_bytes).unwrap());
            first_session.disconnect().await;
            drop(durable);
            drop(credentials);
            drop(provider);

            // A separate native process has no parent handles or plaintext cache.
            // Only synthetic endpoint, opaque key and ciphertext digest cross it.
            let recovery = tokio::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("e2ee::http::tests::native_tag_journal_recovery_child")
                .arg("--ignored")
                .arg("--test-threads=1")
                .env("GRAFT_TEST_TAG_JOURNAL", journal_directory.path())
                .env("GRAFT_TEST_TAG_URL", client.base.as_str())
                .env("GRAFT_TEST_TAG_KEY", &key)
                .env(
                    "GRAFT_TEST_TAG_CANDIDATE_DIGEST",
                    format!("{:x}", Sha256::digest(&exact_candidate)),
                )
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let recovered =
                tokio::time::timeout(Duration::from_secs(20), recovery.wait_with_output())
                    .await
                    .unwrap()
                    .unwrap();
            assert!(
                recovered.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&recovered.stdout),
                String::from_utf8_lossy(&recovered.stderr)
            );

            let fresh_session = self::session();
            let reopened = Arc::new(
                FileRefMutationJournalProvider::open(
                    journal_directory.path(),
                    fresh_session.binding(),
                )
                .unwrap(),
            );
            assert!(
                reopened
                    .journal(fresh_session.binding(), &key)
                    .unwrap()
                    .load(fresh_session.binding())
                    .unwrap()
                    .is_none()
            );
            let credentials = RemoteCredentials::explicit();
            credentials
                .set_http_bearer_token("origin", "account-token".into())
                .unwrap();
            credentials
                .set_encrypted_session_with_journals(
                    "origin",
                    &durable_config,
                    fresh_session.clone(),
                    durable_budget,
                    reopened.clone(),
                )
                .unwrap();
            let restored = crate::remote::Remote::with_config_and_credentials(
                durable_config,
                "origin",
                &credentials,
            )
            .unwrap();
            let before_stale = std::fs::read(&capture_path).unwrap();
            assert!(
                restored
                    .publish_observed_tag_ref(durable_path, &absent, tag_bytes.clone().into())
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&capture_path).unwrap(), before_stale);
            let observed = restored.observe_ref(durable_path).await.unwrap();
            assert!(
                reopened
                    .journal(fresh_session.binding(), &key)
                    .unwrap()
                    .load(fresh_session.binding())
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                restored
                    .read_observed_tag_object(durable_path, &observed)
                    .await
                    .unwrap()
                    .unwrap()
                    .as_ref(),
                tag_bytes
            );
            let captured: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
            let path = format!("/acme/repo/cas/{key}");
            let attempts: Vec<_> = captured
                .as_array()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "POST" && request["path"] == path)
                .collect();
            assert_eq!(
                attempts.len(),
                1,
                "confirmed recovery must not regenerate a tag envelope"
            );
            let encoded = exact_candidate
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(attempts[0]["body"], encoded);
            assert_eq!(attempts[0]["droppedRefMutationResponse"], true);
            assert!(
                captured
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|request| request["droppedRefMutationRead"] == true)
            );
            restored
                .compare_and_delete_observed_ref(durable_path, &observed)
                .await
                .unwrap();
        }
        #[cfg(unix)]
        {
            use super::super::{
                FileMigrationEnrollmentJournal, FileMigrationTargetJournal,
                InitialConfigPublicationOutcome as Outcome, InitializationJournal,
                InitializationStage, MigrationEnrollmentRecord, MigrationInitializationJournal,
            };
            use std::os::unix::fs::PermissionsExt;
            let initial_directory = private_tempdir();
            let target_directory = private_tempdir();
            let enrollment_directory = private_tempdir();
            for path in [
                initial_directory.path(),
                target_directory.path(),
                enrollment_directory.path(),
            ] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let binding = || {
                SessionBinding::new("native.test".into(), "test-account".into(), migration_id)
                    .unwrap()
            };
            let migrating = SecretSession::new(binding(), None).unwrap();
            let initial = migrating
                .prepare_native_initial_config(
                    zeroize::Zeroizing::new(b"migration native fixture password".to_vec()),
                    initial_directory.path().to_path_buf(),
                    true,
                )
                .unwrap()
                .await
                .unwrap();
            let source = repo
                .plan_migration_source(
                    crate::repo::MigrationRefSelection::All,
                    crate::repo::MigrationPlanningBudget::default(),
                )
                .unwrap();
            let mappings = source
                .references()
                .keys()
                .map(|name| (name.clone(), name.clone()))
                .collect();
            let plan = repo
                .plan_migration_target_refs(&source, mappings, source.head().clone())
                .unwrap();
            let target = plan.encode_private().unwrap();
            FileMigrationTargetJournal::create(
                target_directory.path(),
                migrating.binding(),
                &target,
            )
            .unwrap();
            let candidate = initial.load(migrating.binding()).unwrap().config;
            let retained = MigrationEnrollmentRecord::new(
                &migrating.begin_operation().unwrap(),
                candidate.clone(),
                target,
            )
            .unwrap()
            .encode_private()
            .unwrap();
            FileMigrationEnrollmentJournal::create(
                enrollment_directory.path(),
                migrating.binding(),
                &retained,
            )
            .unwrap();
            let guarded = Arc::new(
                MigrationInitializationJournal::new(
                    repo.clone(),
                    enrollment_directory.path(),
                    target_directory.path(),
                    migrating.clone(),
                    initial.clone(),
                )
                .unwrap(),
            );
            let endpoint = ready["migrationUrl"].as_str().unwrap();
            let publishing = self::client(endpoint, migrating.clone());
            assert!(matches!(
                publishing.publish_initial_config(guarded).await.unwrap(),
                Outcome::OutcomeUnknown { .. }
            ));
            assert_eq!(
                initial.load(migrating.binding()).unwrap().stage,
                InitializationStage::Prepared
            );
            migrating.disconnect().await;
            drop(publishing);
            drop(initial);
            let resumed = SecretSession::new(binding(), None).unwrap();
            let initial = resumed
                .prepare_native_initial_config(
                    zeroize::Zeroizing::new(b"migration native fixture password".to_vec()),
                    initial_directory.path().to_path_buf(),
                    false,
                )
                .unwrap()
                .await
                .unwrap();
            assert_eq!(
                initial.load(resumed.binding()).unwrap().config.as_bytes(),
                candidate.as_bytes()
            );
            let guarded = Arc::new(
                MigrationInitializationJournal::new(
                    repo.clone(),
                    enrollment_directory.path(),
                    target_directory.path(),
                    resumed.clone(),
                    initial.clone(),
                )
                .unwrap(),
            );
            let recovery = self::client(endpoint, resumed.clone());
            assert!(matches!(
                recovery
                    .reconcile_initial_config(guarded.clone())
                    .await
                    .unwrap(),
                Outcome::Confirmed
            ));
            assert_eq!(
                initial.load(resumed.binding()).unwrap().stage,
                InitializationStage::ConfigPublished
            );
            let captures: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
            let attempts: Vec<_> = captures
                .as_array()
                .unwrap()
                .iter()
                .filter(|request| {
                    request["method"] == "PUT"
                        && request["path"] == "/acme/migration/raw-if-not-exists/e2ee/config"
                })
                .collect();
            assert_eq!(attempts.len(), 1);
            let exact = candidate
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert_eq!(attempts[0]["body"], exact);
            assert_eq!(attempts[0]["droppedMigrationConfigResponse"], true);
            assert!(
                captures
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|request| request["droppedMigrationConfigRead"] == true)
            );
            // A later source inventory must not be adopted by a retained guard.
            repo.tag_create_annotated(
                "migration-changed-source",
                Some(&committed.id),
                "changed migration source",
            )
            .unwrap();
            let before = std::fs::read(&capture_path).unwrap();
            assert!(matches!(
                recovery.reconcile_initial_config(guarded).await,
                Err(EncryptedHttpError::Encryption(
                    EncryptionError::StaleObservation
                ))
            ));
            assert_eq!(std::fs::read(&capture_path).unwrap(), before);
            assert_eq!(
                FileMigrationEnrollmentJournal::open(
                    enrollment_directory.path(),
                    resumed.binding()
                )
                .unwrap()
                .load(resumed.binding())
                .unwrap(),
                retained
            );
        }
        let remote_config = crate::remote::RemoteConfig::EncryptedHttp {
            url: integration_url.clone(),
            uuid: descriptor()["encryption"]["uuid"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        let source_credentials = RemoteCredentials::explicit();
        source_credentials
            .set_http_bearer_token("origin", "account-token".into())
            .unwrap();
        let budget = crate::remote::EncryptedTransferBudget {
            private_directory: directory.path().to_path_buf(),
            max_buffered_plaintext: 4 * 1024 * 1024,
            max_ciphertext_spool: 8 * 1024 * 1024,
        };
        let source_journal_directory = private_tempdir();
        let source_journals = Arc::new(
            super::super::FileRefMutationJournalProvider::create(
                source_journal_directory.path(),
                session.binding(),
            )
            .unwrap(),
        );
        source_credentials
            .set_encrypted_session_with_journals(
                "origin",
                &remote_config,
                session.clone(),
                budget.clone(),
                source_journals,
            )
            .unwrap();
        let source_config = remote_config.clone();
        let repository_credentials = source_credentials.clone();
        let (source_dir, first_id, source_id, expected_content) =
            tokio::task::spawn_blocking(move || {
                let source_dir = private_tempdir();
                let source = crate::repo::Repository::init(source_dir.path())
                    .unwrap()
                    .with_remote_credentials(repository_credentials.clone());
                source.remote_add("origin", source_config).unwrap();
                let path = source_dir.path().join("repository-private.bin");
                let first_content = b"repository first private content";
                std::fs::write(&path, first_content).unwrap();
                source.stage_artifact_path(&path).unwrap();
                let first = source
                    .commit_staged("repository first private message")
                    .unwrap();
                source.push("origin", "main").unwrap();
                repository_credentials
                    .set_bound_http_bearer_token(
                        "origin",
                        "native.test",
                        "test-account",
                        "renewed-account-token".into(),
                    )
                    .unwrap();
                let mut content = vec![73_u8; 1048609];
                content[..33].copy_from_slice(b"repository second private content");
                std::fs::write(&path, &content).unwrap();
                let state = source.stage_artifact_path(&path).unwrap().artifact.unwrap();
                assert!(state.is_large());
                let second = source
                    .commit_staged("repository second private message")
                    .unwrap();
                let pushed = source.push("origin", "main").unwrap();
                assert_eq!(pushed.head, second.id);
                assert_eq!(source.push("origin", "main").unwrap().commits, 0);
                assert_eq!(
                    source.remote_default_branch("origin").unwrap().as_deref(),
                    Some("main")
                );
                (source_dir, first.id, second.id, content)
            })
            .await
            .unwrap();
        // Independent root/session/password unlock and repository: no source object cache.
        #[cfg(unix)]
        let (target_checkpoint_directory, target_session) = persistent_session();
        #[cfg(not(unix))]
        let target_session = locked_session();
        let target_credentials = RemoteCredentials::explicit();
        target_credentials
            .set_http_bearer_token("origin", "account-token".into())
            .unwrap();
        let target_client = EncryptedHttpClient::new(
            &integration_url,
            "origin".into(),
            target_credentials.clone(),
            target_session.clone(),
        )
        .unwrap();
        target_client
            .unlock(zeroize::Zeroizing::new(b"http sync password".to_vec()))
            .await
            .unwrap();
        let target_journal_directory = private_tempdir();
        let target_journals = Arc::new(
            super::super::FileRefMutationJournalProvider::create(
                target_journal_directory.path(),
                target_session.binding(),
            )
            .unwrap(),
        );
        target_credentials
            .set_encrypted_session_with_journals(
                "origin",
                &remote_config,
                target_session.clone(),
                budget,
                target_journals,
            )
            .unwrap();
        let target_config = remote_config;
        let repository_credentials = target_credentials.clone();
        let expected_first = first_id.clone();
        let expected_second = source_id.clone();
        tokio::task::spawn_blocking(move || {
            let target_dir = private_tempdir();
            let target = crate::repo::Repository::init(target_dir.path())
                .unwrap()
                .with_remote_credentials(repository_credentials.clone());
            target.remote_add("origin", target_config).unwrap();
            let fetched = target.fetch("origin", "main").unwrap();
            assert_eq!(fetched.head, expected_second);
            assert_eq!(fetched.commits, 2);
            assert_eq!(
                target.read_commit(&expected_first).unwrap().message,
                "repository first private message"
            );
            let state =
                target.read_commit(&expected_second).unwrap().artifacts["repository-private.bin"]
                    .clone();
            target
                .materialize_artifact_key("repository-private.bin", &state)
                .unwrap();
            assert_eq!(
                std::fs::read(target_dir.path().join("repository-private.bin")).unwrap(),
                expected_content
            );
            let reopened = crate::repo::Repository::open(target_dir.path())
                .unwrap()
                .with_remote_credentials(repository_credentials);
            assert_eq!(
                reopened
                    .remote_store("origin")
                    .unwrap()
                    .snapshot_upload_concurrency(),
                1
            );
            assert_eq!(
                reopened.read_commit(&expected_second).unwrap().parent,
                Some(expected_first)
            );
        })
        .await
        .unwrap();
        target_credentials
            .clear_encrypted_session("origin")
            .await
            .unwrap();
        assert_eq!(target_session.state(), SecretSessionState::Disconnected);
        #[cfg(unix)]
        {
            let binding = SessionBinding::new(
                "native.test".into(),
                "test-account".into(),
                fixture_root().repository(),
            )
            .unwrap();
            let store = crate::e2ee::FileCheckpointStore::open(
                target_checkpoint_directory.path(),
                &binding,
            )
            .unwrap();
            let restarted = SecretSession::with_checkpoint_store(binding, Arc::new(store)).unwrap();
            assert_eq!(restarted.state(), SecretSessionState::Locked);
            assert_eq!(restarted.checkpoint(), target_session.checkpoint());
            let restarted_client = EncryptedHttpClient::new(
                &integration_url,
                "origin".into(),
                target_credentials.clone(),
                restarted.clone(),
            )
            .unwrap();
            let provider = Arc::new(OnlineFixtureProvider {
                mode: 0,
                loads: std::sync::atomic::AtomicUsize::new(0),
            });
            restarted_client
                .unlock_remembered(provider.clone())
                .await
                .unwrap();
            assert_eq!(provider.loads.load(std::sync::atomic::Ordering::Acquire), 1);
            assert!(
                restarted_client
                    .read_ref("refs/heads/main")
                    .await
                    .unwrap()
                    .target()
                    == Some(&RefTarget::Commit(
                        *blake3::Hash::from_hex(&source_id).unwrap().as_bytes()
                    ))
            );
            restarted.lock().await;
            assert_eq!(restarted.state(), SecretSessionState::Locked);
        }
        assert!(
            crate::remote::RemoteConfig::EncryptedHttp {
                url: format!("{integration_url}-other"),
                uuid: descriptor()["encryption"]["uuid"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            }
            .build_with_credentials("origin", &source_credentials)
            .is_err()
        );
        source_credentials
            .clear_encrypted_session("origin")
            .await
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Disconnected);
        #[cfg(unix)]
        {
            drop(head_journal);
            let (_checkpoint_directory, restarted) = persistent_session();
            let reopened = Arc::new(
                super::super::FileRefMutationJournal::open(
                    head_journal_directory.path(),
                    restarted.binding(),
                )
                .unwrap(),
            );
            let recovery = self::client(ready["url"].as_str().unwrap(), restarted.clone());
            recovery
                .unlock(zeroize::Zeroizing::new(b"http sync password".to_vec()))
                .await
                .unwrap();
            let restored = recovery
                .restore_ref_mutation("HEAD", reopened)
                .await
                .unwrap();
            assert_eq!(restored.expected_bytes(), head.expected_bytes());
            assert_eq!(restored.replacement_bytes(), head.replacement_bytes());
            assert!(matches!(
                recovery.reconcile_ref(&restored).await.unwrap(),
                RefPublicationOutcome::Confirmed
            ));
            restarted.disconnect().await;
        }
        drop(source_dir);
        session.lock().await;
        assert!(matches!(
            adapter.validate_ref_observation("refs/heads/adapter-private", &published),
            Err(crate::remote::RemoteErr::Encryption(_))
        ));
        #[cfg(unix)]
        {
            use crate::e2ee::{
                CheckpointStore, ConfigMutationJournal, ConfigPublicationOutcome,
                FileCheckpointStore, FileConfigMutationJournal,
            };
            use std::os::unix::fs::PermissionsExt;
            let (checkpoint_directory, changing) = persistent_session();
            let changing_client = self::client(ready["url"].as_str().unwrap(), changing.clone());
            changing_client
                .unlock(zeroize::Zeroizing::new(b"http sync password".to_vec()))
                .await
                .unwrap();
            let journal_directory = private_tempdir();
            std::fs::set_permissions(
                journal_directory.path(),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            let journal = Arc::new(
                FileConfigMutationJournal::create(journal_directory.path(), changing.binding())
                    .unwrap(),
            );
            let prepared = changing_client
                .prepare_password_change(
                    zeroize::Zeroizing::new(b"http sync password".to_vec()),
                    zeroize::Zeroizing::new(b"actual service new password".to_vec()),
                )
                .await
                .unwrap();
            let record = prepared
                .record(&changing.begin_operation().unwrap())
                .unwrap();
            let replacement = EncryptedConfig::parse(record.replacement_bytes()).unwrap();
            assert!(matches!(
                changing_client
                    .publish_config_mutation(&prepared, journal.clone())
                    .await
                    .unwrap(),
                ConfigPublicationOutcome::OutcomeUnknown { .. }
            ));
            assert!(journal.load(changing.binding()).unwrap() == Some(record.clone()));
            assert!(
                changing.checkpoint().unwrap() == config().checkpoint(&fixture_root()).unwrap()
            );
            changing.disconnect().await;
            drop(changing_client);
            drop(journal);
            let binding = SessionBinding::new(
                "native.test".into(),
                "test-account".into(),
                fixture_root().repository(),
            )
            .unwrap();
            let checkpoint =
                Arc::new(FileCheckpointStore::open(checkpoint_directory.path(), &binding).unwrap());
            let restarted =
                SecretSession::with_checkpoint_store(binding, checkpoint.clone()).unwrap();
            let recovery = self::client(ready["url"].as_str().unwrap(), restarted.clone());
            assert!(matches!(
                recovery
                    .unlock(zeroize::Zeroizing::new(b"http sync password".to_vec()))
                    .await,
                Err(EncryptedHttpError::Encryption(
                    EncryptionError::UnlockFailed
                ))
            ));
            recovery
                .unlock(zeroize::Zeroizing::new(
                    b"actual service new password".to_vec(),
                ))
                .await
                .unwrap();
            let reopened = Arc::new(
                FileConfigMutationJournal::open(journal_directory.path(), restarted.binding())
                    .unwrap(),
            );
            let restored = recovery
                .restore_config_mutation(reopened.clone())
                .await
                .unwrap();
            assert!(
                restored
                    .record(&restarted.begin_operation().unwrap())
                    .unwrap()
                    == record
            );
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
            assert!(matches!(
                recovery
                    .reconcile_config_mutation(&restored, reopened.clone())
                    .await
                    .unwrap(),
                ConfigPublicationOutcome::Confirmed
            ));
            let after: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
            assert!(
                after.as_array().unwrap()[before.as_array().unwrap().len()..]
                    .iter()
                    .all(|request| request["method"] == "GET")
            );
            assert!(reopened.load(restarted.binding()).unwrap().is_none());
            assert!(
                checkpoint.load(restarted.binding()).unwrap()
                    == Some(replacement.checkpoint(&fixture_root()).unwrap())
            );
            // Existing encrypted objects remain accessible under the unchanged root.
            assert_eq!(
                recovery
                    .read_object_range("store/files/private-upload", 0..16)
                    .await
                    .unwrap()
                    .unwrap(),
                vec![41; 16]
            );
            restarted.disconnect().await;
            let mutations: Vec<_> = after
                .as_array()
                .unwrap()
                .iter()
                .filter(|request| {
                    request["method"] == "POST" && request["path"] == "/acme/repo/cas/e2ee/config"
                })
                .collect();
            assert_eq!(mutations.len(), 1);
            let hex = |bytes: &[u8]| {
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            };
            assert_eq!(mutations[0]["body"], hex(record.replacement_bytes()));
            assert_eq!(mutations[0]["expectedHex"], hex(record.expected_bytes()));
            assert_eq!(mutations[0]["expectedPresent"], "true");
            assert_eq!(mutations[0]["droppedConfigMutationResponse"], true);
            assert!(
                after
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|request| request["droppedConfigMutationRead"] == true)
            );
        }
        child.start_kill().unwrap();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        let captures: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&capture_path).unwrap()).unwrap();
        let requests = captures.as_array().unwrap();
        assert!(requests.len() > 31);
        let primary_requests: Vec<_> = requests
            .iter()
            .filter(|request| request["path"].as_str().unwrap().starts_with("/acme/repo"))
            .collect();
        assert!(primary_requests.len() > 31);
        let upload_path = format!("/acme/repo/raw-if-not-exists/{}", prepared.remote_key());
        let uploads: Vec<_> = primary_requests
            .iter()
            .filter(|request| request["method"] == "PUT" && request["path"] == upload_path)
            .collect();
        assert_eq!(uploads.len(), 2);
        assert_eq!(uploads[0]["body"], uploads[1]["body"]);
        let branch_cas_path = format!("/acme/repo/cas/{}", candidate.remote_key());
        let head_cas_path = format!("/acme/repo/cas/{}", head.remote_key());
        let mutations: Vec<_> = primary_requests
            .iter()
            .filter(|request| {
                request["method"] == "POST"
                    && (request["path"] == branch_cas_path || request["path"] == head_cas_path)
            })
            .collect();
        assert_eq!(mutations.len(), 3);
        assert_eq!(mutations[0]["body"], mutations[1]["body"]);
        assert!(
            captures
                .as_array()
                .unwrap()
                .iter()
                .all(|request| request["authenticated"] == true)
        );
        let text = captures.to_string();
        for id in streamed_object_ids {
            assert!(!text.contains(&id), "migration object identity leaked");
        }
        for hidden in [
            "private.pack",
            "private.idx",
            "private-name",
            "private-annotated-tag",
            "adapter private annotation",
            "adapter-private-release",
            "adapter-private-light",
            "adapter-false-content",
            "durable-private-release",
            "migration native fixture password",
            "migration-private-large.bin",
            "migration streamed private content",
            "migration stream source message",
            "migration-packed-tag",
            "migration packed tag message",
            &streamed_file_id,
            "migration-changed-source",
            "changed migration source",
            &annotated.object,
            &"1f".repeat(32),
            &"13".repeat(32),
            "private-upload",
            "adapter-private",
            "adapter-unpublished",
            "adapter private unpublished marker",
            "adapter private commit message",
            "http sync password",
            "actual service new password",
            &"07".repeat(32),
            &pack_id,
            &committed.id,
            &first_id,
            &source_id,
            "repository-private.bin",
            "repository first private message",
            "repository second private message",
            "repository first private content",
            "repository second private content",
        ] {
            assert!(!text.contains(hidden));
            let encoded = hidden
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert!(
                !text.contains(&encoded),
                "plaintext marker appears in a captured binary body"
            );
        }
        let root_marker = (0_u8..32)
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(!text.contains(&root_marker));
    }

    async fn write_reply(stream: &mut tokio::net::TcpStream, response: &Reply) {
        let headers = format!(
            "HTTP/1.1 {} Test\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
            response.status,
            response.headers,
            response.bytes.len()
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(&response.bytes).await.unwrap();
    }

    fn download_replies(header: &[u8], payload: &[Vec<u8>]) -> (Vec<Reply>, Reply) {
        let mut wire = header.to_vec();
        for chunk in payload {
            wire.extend_from_slice(chunk);
        }
        let replies = vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            range_reply(&wire, 0..72),
            range_reply(&wire, 72..header.len()),
        ];
        (replies, range_reply(&wire, header.len()..wire.len()))
    }

    #[derive(Clone, Copy)]
    enum FullBodyFraming {
        ContentLength,
        Chunked,
        Truncated,
    }

    async fn fragmented_download_server(
        prefix: Vec<Reply>,
        payload: Reply,
        framing: FullBodyFraming,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/repo", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in prefix {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut stream).await);
                write_reply(&mut stream, &response).await;
            }
            let (mut stream, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut stream).await);
            let chunked = matches!(framing, FullBodyFraming::Chunked);
            let framing_header = if chunked {
                "Transfer-Encoding: chunked\r\n".to_owned()
            } else {
                format!("Content-Length: {}\r\n", payload.bytes.len())
            };
            let headers = format!(
                "HTTP/1.1 {} Test\r\n{}{}Connection: close\r\n\r\n",
                payload.status, payload.headers, framing_header
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            let end =
                payload.bytes.len() - usize::from(matches!(framing, FullBodyFraming::Truncated));
            for fragment in payload.bytes[..end].chunks(113) {
                if chunked
                    && stream
                        .write_all(format!("{:x}\r\n", fragment.len()).as_bytes())
                        .await
                        .is_err()
                {
                    break;
                }
                if stream.write_all(fragment).await.is_err() {
                    break;
                }
                if chunked && stream.write_all(b"\r\n").await.is_err() {
                    break;
                }
            }
            if chunked {
                let _ = stream.write_all(b"0\r\n\r\n").await;
            }
            requests
        });
        (url, served)
    }

    #[tokio::test]
    async fn streamed_download_verifies_empty_objects_all_sizes_and_private_file_handoff() {
        use std::io::Read;
        for (size, length) in [
            (super::super::ChunkSize::KiB16, 0),
            (super::super::ChunkSize::KiB16, 16385),
            (super::super::ChunkSize::KiB64, 65539),
            (super::super::ChunkSize::KiB256, 262145),
        ] {
            let plaintext: Vec<u8> = (0..length).map(|i| (i % 251) as u8).collect();
            let (header, chunks, _) = super::super::tests::encrypted_object(
                &fixture_root(),
                "segments/private",
                &plaintext,
                size,
            );
            let (prefix, payload) = download_replies(&header, &chunks);
            let (url, served) =
                fragmented_download_server(prefix, payload, FullBodyFraming::Chunked).await;
            let directory = private_tempdir();
            let downloaded = client(&url, session())
                .download_object("segments/private", directory.path(), length as u64)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(downloaded.metadata().plaintext_length, length as u64);
            assert_eq!(
                downloaded.metadata().plaintext_sha256,
                <[u8; 32]>::from(Sha256::digest(&plaintext))
            );
            let mut file = downloaded.into_file();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
            }
            let mut output = Vec::new();
            file.read_to_end(&mut output).unwrap();
            assert_eq!(output, plaintext);
            drop(file);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
            assert_eq!(served.await.unwrap().len(), 5);
        }
    }

    #[tokio::test]
    async fn failed_streamed_downloads_never_handoff_and_clean_private_storage() {
        for case in 0..6 {
            let plaintext: &[u8] = if case == 5 { b"" } else { b"content" };
            let (header, mut chunks, _) = super::super::tests::encrypted_object(
                &fixture_root(),
                "segments/private",
                plaintext,
                super::super::ChunkSize::KiB64,
            );
            if case == 0 || case == 5 {
                chunks[0][0] ^= 1;
            }
            let (prefix, mut payload) = download_replies(&header, &chunks);
            let framing = if case == 1 {
                FullBodyFraming::Truncated
            } else {
                FullBodyFraming::Chunked
            };
            if case == 2 {
                payload.bytes.push(0);
            }
            let directory = private_tempdir();
            let result = if matches!(case, 3 | 4) {
                let (url, served) = server(prefix).await;
                let path = if case == 3 {
                    directory.path().to_path_buf()
                } else {
                    directory.path().join("missing-private-cache")
                };
                let cap = if case == 3 { 6 } else { 7 };
                let result = client(&url, session())
                    .download_object("segments/private", &path, cap)
                    .await;
                assert_eq!(served.await.unwrap().len(), 4);
                result
            } else {
                let (url, served) = fragmented_download_server(prefix, payload, framing).await;
                let result = client(&url, session())
                    .download_object("segments/private", directory.path(), 7)
                    .await;
                assert_eq!(served.await.unwrap().len(), 5);
                result
            };
            assert!(result.is_err());
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn streamed_download_rejects_authenticated_false_digest() {
        let mut writer = super::super::ObjectEncryptor::new(
            &fixture_root(),
            "segments/private",
            7,
            [0; 32],
            super::super::ChunkSize::KiB64,
        )
        .unwrap();
        let header = writer.header_bytes().to_vec();
        let chunk = writer.encrypt_next(b"content").unwrap();
        let (prefix, payload) = download_replies(&header, &[chunk]);
        let (url, served) =
            fragmented_download_server(prefix, payload, FullBodyFraming::ContentLength).await;
        let directory = private_tempdir();
        assert!(matches!(
            client(&url, session())
                .download_object("segments/private", directory.path(), 7)
                .await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::SnapshotMismatch
            ))
        ));
        assert_eq!(served.await.unwrap().len(), 5);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn streamed_download_exceeds_range_memory_cap_with_one_payload_request() {
        use std::io::Read;
        let block = vec![91; 65536];
        let length = MAX_RANGE_PLAINTEXT + 13;
        let mut hash = Sha256::new();
        for _ in 0..length / block.len() as u64 {
            hash.update(&block);
        }
        hash.update(&block[..13]);
        let expected: [u8; 32] = hash.finalize().into();
        let mut writer = super::super::ObjectEncryptor::new(
            &fixture_root(),
            "segments/large",
            length,
            expected,
            super::super::ChunkSize::KiB64,
        )
        .unwrap();
        let header = writer.header_bytes().to_vec();
        let total = writer.ciphertext_length().unwrap();
        let replies = vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            Reply {
                status: 206,
                headers: format!("Graft-Protocol: 1\r\nContent-Range: bytes 0-71/{total}\r\n"),
                bytes: header[..72].to_vec(),
            },
            Reply {
                status: 206,
                headers: format!(
                    "Graft-Protocol: 1\r\nContent-Range: bytes 72-{}/{total}\r\n",
                    header.len() - 1
                ),
                bytes: header[72..].to_vec(),
            },
        ];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/repo", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            for response in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                write_reply(&mut stream, &response).await;
            }
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(request.to_lowercase().contains(&format!(
                "range: bytes={}-{}",
                header.len(),
                total - 1
            )));
            let headers = format!(
                "HTTP/1.1 206 Test\r\nGraft-Protocol: 1\r\nContent-Range: bytes {}-{}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                header.len(),
                total - 1,
                total - header.len() as u64
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            for _ in 0..length / block.len() as u64 {
                stream
                    .write_all(&writer.encrypt_next(&block).unwrap())
                    .await
                    .unwrap();
            }
            stream
                .write_all(&writer.encrypt_next(&block[..13]).unwrap())
                .await
                .unwrap();
            writer.finish().unwrap();
        });
        let directory = private_tempdir();
        let downloaded = client(&url, session())
            .download_object("segments/large", directory.path(), length)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(downloaded.metadata().plaintext_length, length);
        let mut file = downloaded.into_file();
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 65536];
        loop {
            let count = file.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        assert_eq!(<[u8; 32]>::from(hash.finalize()), expected);
        served.await.unwrap();
    }

    #[tokio::test]
    async fn discovery_cache_reauthenticates_and_never_substitutes_for_content_reads() {
        let logical = "store/files/discovery-cache-private";
        let (header, chunks, _) = super::super::tests::encrypted_object(
            &fixture_root(),
            logical,
            b"discovery cache payload",
            super::super::ChunkSize::KiB64,
        );
        let header_end = header.len();
        let mut wire = header;
        for chunk in chunks {
            wire.extend(chunk);
        }
        let mut corrupted = wire.clone();
        *corrupted.last_mut().unwrap() ^= 1;
        let handshake = || {
            vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
            ]
        };
        let mut responses = handshake();
        responses.extend([
            range_reply(&wire, 0..72),
            range_reply(&wire, 72..header_end),
        ]);
        responses.extend(handshake());
        responses.extend([
            range_reply(&corrupted, 0..72),
            range_reply(&corrupted, 72..header_end),
            range_reply(&corrupted, header_end..corrupted.len()),
        ]);
        responses.extend(handshake());
        responses.extend([
            range_reply(&wire, 0..72),
            range_reply(&wire, 72..header_end),
        ]);
        let (url, served) = server(responses).await;
        let session = session();
        let key = object::remote_key(&fixture_root(), logical).unwrap();
        let first = client(&url, session.clone());
        assert_eq!(
            first
                .discover_object_cached(&key)
                .await
                .unwrap()
                .unwrap()
                .logical_key,
            logical
        );
        let rebuilt = client(&url, session.clone());
        assert_eq!(
            rebuilt
                .discover_object_cached(&key)
                .await
                .unwrap()
                .unwrap()
                .logical_key,
            logical
        );
        assert!(matches!(
            rebuilt.read_object_range(logical, 0..23).await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::AuthenticationFailed
            ))
        ));
        session.lock().await;
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        assert_eq!(
            client(&url, session)
                .discover_object_cached(&key)
                .await
                .unwrap()
                .unwrap()
                .logical_key,
            logical
        );
        let requests = served.await.unwrap();
        assert_eq!(requests.len(), 13);
    }

    #[tokio::test]
    async fn discovery_cache_rejects_corrupt_cached_header() {
        let logical = "store/files/cache-corrupt-private";
        let (mut header, chunks, _) = super::super::tests::encrypted_object(
            &fixture_root(),
            logical,
            b"payload",
            super::super::ChunkSize::KiB64,
        );
        let total = header.len() + chunks.iter().map(Vec::len).sum::<usize>();
        *header.last_mut().unwrap() ^= 1;
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
        ])
        .await;
        let session = session();
        let client = client(&url, session.clone());
        let key = object::remote_key(&fixture_root(), logical).unwrap();
        session
            .begin_operation()
            .unwrap()
            .cache_discovery_header(client.base.as_str(), &key, header, total as u64)
            .unwrap();
        assert!(matches!(
            client.discover_object_cached(&key).await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::AuthenticationFailed
            ))
        ));
        assert_eq!(served.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn discovery_cache_never_proves_remote_existence() {
        let logical = "store/files/cache-deleted-private";
        let (header, chunks, _) = super::super::tests::encrypted_object(
            &fixture_root(),
            logical,
            b"payload",
            super::super::ChunkSize::KiB64,
        );
        let header_end = header.len();
        let mut wire = header;
        for chunk in chunks {
            wire.extend(chunk);
        }
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            range_reply(&wire, 0..72),
            range_reply(&wire, 72..header_end),
            Reply {
                status: 404,
                headers: "Graft-Protocol: 1\r\n".into(),
                bytes: vec![],
            },
        ])
        .await;
        let client = client(&url, session());
        let key = object::remote_key(&fixture_root(), logical).unwrap();
        assert!(client.discover_object_cached(&key).await.unwrap().is_some());
        assert!(client.discover_object(&key).await.unwrap().is_none());
        assert_eq!(served.await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn native_object_ranges_authenticate_all_sizes_and_discover_without_logical_names() {
        for (size, block) in [
            (super::super::ChunkSize::KiB16, 16384),
            (super::super::ChunkSize::KiB64, 65536),
            (super::super::ChunkSize::KiB256, 262144),
        ] {
            let plaintext: Vec<u8> = (0..2 * block + 20).map(|i| (i % 251) as u8).collect();
            let logical = "store/files/private-document";
            let (header, chunks, _) =
                super::super::tests::encrypted_object(&fixture_root(), logical, &plaintext, size);
            let header_end = header.len();
            let mut wire = header;
            for chunk in chunks {
                wire.extend(chunk);
            }
            let key = object::remote_key(&fixture_root(), logical).unwrap();
            let mut replies = vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
            ];
            for covered in [
                Some(header_end..wire.len()),
                Some(header_end..header_end + 2 * (block + 16)),
                None,
                None,
            ] {
                replies.push(range_reply(&wire, 0..72));
                replies.push(range_reply(&wire, 72..header_end));
                if let Some(covered) = covered {
                    replies.push(range_reply(&wire, covered));
                }
            }
            let (url, served) = server(replies).await;
            let client = client(&url, session());
            assert_eq!(
                client
                    .read_object_range(logical, 0..plaintext.len() as u64)
                    .await
                    .unwrap()
                    .unwrap(),
                plaintext
            );
            assert_eq!(
                client
                    .read_object_range(logical, (block - 2) as u64..(block + 2) as u64)
                    .await
                    .unwrap()
                    .unwrap(),
                plaintext[block - 2..block + 2]
            );
            assert!(
                client
                    .read_object_range(logical, 0..0)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_empty()
            );
            let metadata = client.discover_object(&key).await.unwrap().unwrap();
            assert_eq!(metadata.logical_key, logical);
            assert_eq!(metadata.plaintext_length, plaintext.len() as u64);
            assert_eq!(
                metadata.plaintext_sha256,
                <[u8; 32]>::from(Sha256::digest(&plaintext))
            );
            let requests = served.await.unwrap();
            assert_eq!(requests.len(), 12);
            for request in &requests[2..] {
                assert!(request.starts_with(&format!("GET /alice/repo/raw/{key} HTTP/1.1")));
                assert!(request.to_lowercase().contains("accept-encoding: identity"));
                assert!(!request.contains("private-document") && !request.contains("store/files"));
            }
            assert!(requests[4].to_lowercase().contains(&format!(
                "range: bytes={}-{}",
                header_end,
                wire.len() - 1
            )));
            assert!(requests[7].to_lowercase().contains(&format!(
                "range: bytes={}-{}",
                header_end,
                header_end + 2 * (block + 16) - 1
            )));
        }
    }

    #[tokio::test]
    async fn object_ranges_reject_ignored_ranges_bad_bounds_and_payload_corruption() {
        let (header, chunks, _) = super::super::tests::encrypted_object(
            &fixture_root(),
            "segments/private",
            b"content",
            super::super::ChunkSize::KiB64,
        );
        let header_end = header.len();
        let mut wire = header;
        for chunk in chunks {
            wire.extend(chunk);
        }
        for case in 0..5 {
            let mut prefix = range_reply(&wire, 0..72);
            match case {
                0 => prefix.status = 200,
                1 => {
                    prefix.headers = format!(
                        "Graft-Protocol: 1\r\nContent-Range: bytes 1-72/{}\r\n",
                        wire.len()
                    )
                }
                2 => prefix
                    .headers
                    .push_str("Content-Range: bytes 0-71/1000\r\n"),
                3 => prefix.headers.push_str("Content-Encoding: gzip\r\n"),
                _ => {}
            }
            let mut replies = vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
                prefix,
            ];
            if case == 4 {
                replies.push(range_reply(&wire, 72..header_end));
                let mut payload = range_reply(&wire, header_end..wire.len());
                payload.bytes[0] ^= 1;
                replies.push(payload);
            }
            let count = replies.len();
            let (url, served) = server(replies).await;
            let result = client(&url, session())
                .read_object_range("segments/private", 0..7)
                .await;
            assert!(result.is_err());
            assert_eq!(served.await.unwrap().len(), count);
        }
        let client = client("http://127.0.0.1:1/repo", session());
        assert!(matches!(
            client
                .read_object_range("segments/private", 0..MAX_RANGE_PLAINTEXT + 1)
                .await,
            Err(EncryptedHttpError::Encryption(EncryptionError::TooLarge))
        ));
    }

    #[test]
    fn content_range_parser_rejects_unknown_totals_overflow_and_wrong_intervals() {
        assert_eq!(
            parse_content_range("bytes 0-71/200", &(0..72)).unwrap(),
            200
        );
        for value in [
            "bytes 0-71/*",
            "bytes */200",
            "bytes 0-71/71",
            "bytes +0-71/200",
            "bytes 0-72/200",
            "bytes 0-71/18446744073709551616",
            "bytes 0-71/200/300",
            "bytes 0-71/200 ",
        ] {
            assert!(parse_content_range(value, &(0..72)).is_err());
        }
    }

    #[tokio::test]
    async fn full_object_range_rejects_authenticated_header_with_wrong_plaintext_digest() {
        // A key-owning faulty publisher can authenticate a false digest; readers still verify it.
        let mut writer = super::super::ObjectEncryptor::new(
            &fixture_root(),
            "segments/test",
            7,
            [0; 32],
            super::super::ChunkSize::KiB64,
        )
        .unwrap();
        let mut wire = writer.header_bytes().to_vec();
        let header_end = wire.len();
        wire.extend(writer.encrypt_next(b"content").unwrap());
        assert!(writer.finish().is_err());
        let replies = vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            range_reply(&wire, 0..72),
            range_reply(&wire, 72..header_end),
            range_reply(&wire, header_end..wire.len()),
        ];
        let (url, served) = server(replies).await;
        assert!(matches!(
            client(&url, session())
                .read_object_range("segments/test", 0..7)
                .await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::SnapshotMismatch
            ))
        ));
        assert_eq!(served.await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn lock_cancels_partial_payload_download_with_authenticated_reader_alive() {
        for (full, abort) in [(false, false), (true, false), (true, true)] {
            let (header, chunks, _) = super::super::tests::encrypted_object(
                &fixture_root(),
                "segments/test",
                b"content",
                super::super::ChunkSize::KiB64,
            );
            let header_end = header.len();
            let mut wire = header;
            for chunk in chunks {
                wire.extend(chunk);
            }
            let mut replies = vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
                range_reply(&wire, 0..72),
                range_reply(&wire, 72..header_end),
                range_reply(&wire, header_end..wire.len()),
            ];
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/repo", listener.local_addr().unwrap());
            let (started, receiving) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let served = tokio::spawn(async move {
                for response in replies.drain(..4) {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(stream.read_u8().await.unwrap());
                    }
                    let headers = format!(
                        "HTTP/1.1 {} Test\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        response.status,
                        response.headers,
                        response.bytes.len()
                    );
                    stream.write_all(headers.as_bytes()).await.unwrap();
                    stream.write_all(&response.bytes).await.unwrap();
                }
                let response = replies.remove(0);
                let (mut stream, _) = listener.accept().await.unwrap();
                let headers = format!(
                    "HTTP/1.1 206 Test\r\n{}Content-Length: {}\r\n\r\n",
                    response.headers,
                    response.bytes.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(&response.bytes[..1]).await.unwrap();
                started.send(()).unwrap();
                released.await.unwrap();
            });
            let session = session();
            let client = client(&url, session.clone());
            let directory = private_tempdir();
            let path = directory.path().to_path_buf();
            let reading = tokio::spawn(async move {
                if full {
                    client
                        .download_object("segments/test", &path, 7)
                        .await
                        .map(|value| value.is_some())
                } else {
                    client
                        .read_object_range("segments/test", 0..7)
                        .await
                        .map(|value| value.is_some())
                }
            });
            tokio::time::timeout(Duration::from_secs(2), receiving)
                .await
                .unwrap()
                .unwrap();
            if abort {
                reading.abort();
                assert!(reading.await.unwrap_err().is_cancelled());
                tokio::time::timeout(Duration::from_secs(2), session.lock())
                    .await
                    .unwrap();
            } else {
                tokio::time::timeout(Duration::from_secs(2), session.lock())
                    .await
                    .unwrap();
                assert!(matches!(
                    reading.await.unwrap(),
                    Err(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
                ));
            }
            assert_eq!(session.state(), SecretSessionState::Locked);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
            release.send(()).unwrap();
            served.await.unwrap();
        }
    }

    #[tokio::test]
    async fn first_device_unlock_fetches_config_and_reuses_verified_epoch() {
        let sealed = EncryptedRef::seal(
            &fixture_root(),
            "refs/heads/private-name",
            &RefTarget::Commit([7; 32]),
        )
        .unwrap();
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            reply(sealed.as_bytes().to_vec()),
        ])
        .await;
        let session = locked_session();
        let client = client(&url, session.clone());
        client
            .unlock(zeroize::Zeroizing::new(b"http sync password".to_vec()))
            .await
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Unlocked);
        assert!(session.checkpoint().is_some());
        let observation = client.read_ref("refs/heads/private-name").await.unwrap();
        assert_eq!(observation.expected_bytes(), Some(sealed.as_bytes()));
        let requests = served.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("GET /alice/repo/raw/e2ee/config HTTP/1.1"));
        for request in requests {
            assert!(!request.contains("http sync password") && !request.contains("private-name"));
        }
    }

    struct OnlineFixtureProvider {
        mode: u8,
        loads: std::sync::atomic::AtomicUsize,
    }

    #[tokio::test]
    async fn prepared_unlock_registration_cannot_cross_lock_before_host_scheduling() {
        for remembered in [false, true] {
            let session = locked_session();
            let client = client("http://127.0.0.1:1/alice/repo", session.clone());
            let provider = Arc::new(OnlineFixtureProvider {
                mode: 0,
                loads: std::sync::atomic::AtomicUsize::new(0),
            });
            let prepare =
                || -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
                    if remembered {
                        Box::pin(client.prepare_remembered_unlock(provider.clone()).unwrap())
                    } else {
                        Box::pin(
                            client
                                .prepare_password_unlock(zeroize::Zeroizing::new(
                                    b"synthetic password".to_vec(),
                                ))
                                .unwrap(),
                        )
                    }
                };
            let abandoned = prepare();
            assert_eq!(session.state(), SecretSessionState::Unlocking);
            drop(abandoned);
            assert_eq!(session.state(), SecretSessionState::Locked);
            let queued = prepare();
            let closing_session = session.clone();
            let closing = tokio::spawn(async move { closing_session.lock().await });
            tokio::time::timeout(Duration::from_secs(1), async {
                while session.state() != SecretSessionState::Locking {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!closing.is_finished());
            assert!(matches!(
                queued.await,
                Err(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
            ));
            closing.await.unwrap();
            assert_eq!(session.state(), SecretSessionState::Locked);
            assert!(session.checkpoint().is_none());
            assert_eq!(provider.loads.load(std::sync::atomic::Ordering::Acquire), 0);
        }
    }
    impl RememberedKeyProvider for OnlineFixtureProvider {
        fn load(
            &self,
            binding: &SessionBinding,
        ) -> crate::e2ee::Result<Option<crate::e2ee::RootKey>> {
            self.loads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            match self.mode {
                0 => Ok(Some(fixture_root())),
                1 => Ok(None),
                2 => Err(EncryptionError::SecureStorageUnavailable),
                _ => Ok(Some(crate::e2ee::RootKey::generate(binding.repository())?)),
            }
        }
        fn remember(
            &self,
            _: &SessionBinding,
            _: &crate::e2ee::RootKey,
        ) -> crate::e2ee::Result<()> {
            unreachable!("online unlock cannot save keys")
        }
        fn forget(&self, _: &SessionBinding) -> crate::e2ee::Result<()> {
            unreachable!("online unlock cannot delete keys")
        }
    }

    #[tokio::test]
    async fn remembered_online_unlock_authenticates_before_data_and_reuses_epoch() {
        for mode in 0..4 {
            let sealed = EncryptedRef::seal(
                &fixture_root(),
                "refs/heads/private-name",
                &RefTarget::Commit([7; 32]),
            )
            .unwrap();
            let mut replies = vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(config().as_bytes().to_vec()),
            ];
            if mode == 0 {
                replies.push(reply(sealed.as_bytes().to_vec()));
            }
            let (url, served) = server(replies).await;
            let session = locked_session();
            let client = client(&url, session.clone());
            let provider = Arc::new(OnlineFixtureProvider {
                mode,
                loads: std::sync::atomic::AtomicUsize::new(0),
            });
            let result = client.unlock_remembered(provider.clone()).await;
            if mode == 0 {
                result.unwrap();
                assert_eq!(session.state(), SecretSessionState::Unlocked);
                assert!(session.checkpoint().is_some());
                let observation = client.read_ref("refs/heads/private-name").await.unwrap();
                assert_eq!(observation.expected_bytes(), Some(sealed.as_bytes()));
            } else {
                let expected = match mode {
                    1 => EncryptionError::RememberedKeyMissing,
                    2 => EncryptionError::SecureStorageUnavailable,
                    _ => EncryptionError::AuthenticationFailed,
                };
                assert!(
                    matches!(result, Err(EncryptedHttpError::Encryption(error)) if error == expected)
                );
                assert_eq!(session.state(), SecretSessionState::Locked);
                assert!(session.checkpoint().is_none());
                assert!(client.verified.lock().await.is_none());
            }
            assert_eq!(provider.loads.load(std::sync::atomic::Ordering::Acquire), 1);
            let requests = served.await.unwrap();
            assert_eq!(requests.len(), if mode == 0 { 3 } else { 2 });
            assert!(requests[1].starts_with("GET /alice/repo/raw/e2ee/config HTTP/1.1"));
            for request in requests {
                assert!(
                    !request.contains("private-name") && !request.contains("http sync password")
                );
            }
        }
    }

    #[tokio::test]
    async fn remembered_online_unlock_rejects_foreign_identity_before_provider_load() {
        let mut foreign_descriptor = descriptor();
        foreign_descriptor["encryption"]["uuid"] =
            serde_json::json!("00000000-0000-4000-8000-000000000000");
        let mut foreign_config = config().as_bytes().to_vec();
        foreign_config[12..28]
            .copy_from_slice(crate::e2ee::RepositoryId::generate().unwrap().as_bytes());
        for replies in [
            vec![reply(serde_json::to_vec(&foreign_descriptor).unwrap())],
            vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                reply(foreign_config),
            ],
        ] {
            let count = replies.len();
            let (url, served) = server(replies).await;
            let session = locked_session();
            let client = client(&url, session.clone());
            let provider = Arc::new(OnlineFixtureProvider {
                mode: 0,
                loads: std::sync::atomic::AtomicUsize::new(0),
            });
            assert!(matches!(
                client.unlock_remembered(provider.clone()).await,
                Err(EncryptedHttpError::Encryption(
                    EncryptionError::IdentityChanged
                ))
            ));
            assert_eq!(provider.loads.load(std::sync::atomic::Ordering::Acquire), 0);
            assert_eq!(session.state(), SecretSessionState::Locked);
            assert!(session.checkpoint().is_none());
            assert!(client.verified.lock().await.is_none());
            assert_eq!(served.await.unwrap().len(), count);
        }
    }

    #[tokio::test]
    async fn checkpoint_persistence_failure_stops_before_ref_request_and_profile_cache() {
        struct Store(std::sync::atomic::AtomicBool);
        impl crate::e2ee::CheckpointStore for Store {
            fn load(
                &self,
                _: &crate::e2ee::SessionBinding,
            ) -> crate::e2ee::Result<Option<crate::e2ee::ConfigCheckpoint>> {
                Ok(None)
            }
            fn advance(
                &self,
                _: &crate::e2ee::SessionBinding,
                _: &crate::e2ee::ConfigCheckpoint,
            ) -> crate::e2ee::Result<()> {
                if self.0.load(std::sync::atomic::Ordering::Acquire) {
                    Err(EncryptionError::CheckpointUnavailable(
                        std::io::ErrorKind::PermissionDenied,
                    ))
                } else {
                    Ok(())
                }
            }
        }
        let store = Arc::new(Store(std::sync::atomic::AtomicBool::new(false)));
        let session = SecretSession::with_checkpoint_store(
            crate::e2ee::SessionBinding::new(
                "native.test".into(),
                "test-account".into(),
                fixture_root().repository(),
            )
            .unwrap(),
            store.clone(),
        )
        .unwrap();
        session.unlock_with_root(&config(), fixture_root()).unwrap();
        let previous = session.checkpoint();
        store.0.store(true, std::sync::atomic::Ordering::Release);
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
        ])
        .await;
        let client = client(&url, session.clone());
        assert!(matches!(
            client.read_ref("refs/heads/private-name").await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::CheckpointUnavailable(_)
            ))
        ));
        let requests = served.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("GET /alice/repo/raw/e2ee/config HTTP/1.1"));
        assert!(client.verified.lock().await.is_none());
        assert_eq!(session.checkpoint(), previous);
        session.lock().await;
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
        ])
        .await;
        let remembered_client = self::client(&url, session.clone());
        assert!(matches!(
            remembered_client
                .unlock_remembered(Arc::new(OnlineFixtureProvider {
                    mode: 0,
                    loads: std::sync::atomic::AtomicUsize::new(0),
                }))
                .await,
            Err(EncryptedHttpError::Encryption(
                EncryptionError::CheckpointUnavailable(_)
            ))
        ));
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert_eq!(session.checkpoint(), previous);
        assert!(remembered_client.verified.lock().await.is_none());
        assert_eq!(served.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn first_device_unlock_failures_restore_locked_without_data_reads() {
        let mut foreign = descriptor();
        foreign["encryption"]["uuid"] = serde_json::json!("123e4567-e89b-42d3-a456-426614174000");
        let cases = vec![
            (vec![reply(serde_json::to_vec(&foreign).unwrap())], 0),
            (
                vec![
                    reply(serde_json::to_vec(&descriptor()).unwrap()),
                    Reply {
                        status: 404,
                        headers: "Graft-Protocol: 1\r\n".into(),
                        bytes: Vec::new(),
                    },
                ],
                1,
            ),
            (
                vec![
                    reply(serde_json::to_vec(&descriptor()).unwrap()),
                    reply(config().as_bytes().to_vec()),
                ],
                2,
            ),
        ];
        for (replies, case) in cases {
            let count = replies.len();
            let (url, served) = server(replies).await;
            let session = locked_session();
            let result = client(&url, session.clone())
                .unlock(zeroize::Zeroizing::new(b"wrong password".to_vec()))
                .await;
            match case {
                0 => assert!(matches!(
                    result,
                    Err(EncryptedHttpError::Encryption(
                        EncryptionError::IdentityChanged
                    ))
                )),
                1 => assert!(matches!(result, Err(EncryptedHttpError::ConfigMissing))),
                _ => assert!(matches!(
                    result,
                    Err(EncryptedHttpError::Encryption(
                        EncryptionError::UnlockFailed
                    ))
                )),
            }
            assert_eq!(session.state(), SecretSessionState::Locked);
            assert!(session.checkpoint().is_none());
            assert_eq!(served.await.unwrap().len(), count);
        }
    }

    #[tokio::test]
    async fn paused_first_device_unlock_succeeds_without_admitting_sync() {
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
        ])
        .await;
        let session = locked_session();
        session.pause(true);
        let client = client(&url, session.clone());
        client
            .unlock(zeroize::Zeroizing::new(b"http sync password".to_vec()))
            .await
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Unlocked);
        assert!(session.is_paused());
        assert!(matches!(
            client.verify().await,
            Err(EncryptedHttpError::Encryption(EncryptionError::Paused))
        ));
        assert_eq!(served.await.unwrap().len(), 2);
        assert!(client.verified.lock().await.is_none());
    }

    #[tokio::test]
    async fn first_device_download_lock_and_caller_abort_release_unlock_ticket() {
        for (abort, config_download, remembered) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (false, false, true),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/alice/repo", listener.local_addr().unwrap());
            let session = locked_session();
            let client = client(&url, session.clone());
            let provider = Arc::new(OnlineFixtureProvider {
                mode: 0,
                loads: std::sync::atomic::AtomicUsize::new(0),
            });
            let input_provider = provider.clone();
            let unlocking = tokio::spawn(async move {
                if remembered {
                    client.unlock_remembered(input_provider).await
                } else {
                    client
                        .unlock(zeroize::Zeroizing::new(b"http sync password".to_vec()))
                        .await
                }
            });
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
            if config_download {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                let body = serde_json::to_vec(&descriptor()).unwrap();
                let headers = format!(
                    "HTTP/1.1 200 Test\r\nGraft-Protocol: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
                drop(stream);
                (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                stream.write_all(b"HTTP/1.1 200 Test\r\nGraft-Protocol: 1\r\nContent-Length: 206\r\n\r\nGRAFTEC1").await.unwrap();
            }
            assert_eq!(session.state(), SecretSessionState::Unlocking);
            if abort {
                unlocking.abort();
                assert!(unlocking.await.unwrap_err().is_cancelled());
            } else {
                tokio::time::timeout(Duration::from_secs(2), session.lock())
                    .await
                    .unwrap();
                assert!(matches!(
                    unlocking.await.unwrap(),
                    Err(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
                ));
            }
            assert_eq!(session.state(), SecretSessionState::Locked);
            assert!(session.checkpoint().is_none());
            assert_eq!(provider.loads.load(std::sync::atomic::Ordering::Acquire), 0);
            drop(stream);
        }
    }

    #[tokio::test]
    async fn native_http_authenticates_config_and_refs_without_transmitting_logical_names() {
        let sealed = EncryptedRef::seal(
            &fixture_root(),
            "refs/heads/private-name",
            &RefTarget::Commit([7; 32]),
        )
        .unwrap();
        let (url, served) = server(vec![
            reply(serde_json::to_vec(&descriptor()).unwrap()),
            reply(config().as_bytes().to_vec()),
            reply(sealed.as_bytes().to_vec()),
            Reply {
                status: 404,
                headers: "Graft-Protocol: 1\r\n".into(),
                bytes: Vec::new(),
            },
        ])
        .await;
        let session = session();
        let client = client(&url, session.clone());
        let observation = client.read_ref("refs/heads/private-name").await.unwrap();
        assert!(observation.target() == Some(&RefTarget::Commit([7; 32])));
        assert_eq!(observation.expected_bytes(), Some(sealed.as_bytes()));
        assert_eq!(observation.remote_key(), sealed.remote_key());
        let absent = client.read_ref("refs/heads/absent-name").await.unwrap();
        assert!(absent.expected_bytes().is_none());
        let requests = tokio::time::timeout(Duration::from_secs(2), served)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 4); // Descriptor and config are shared within one unlock epoch.
        assert!(requests[0].starts_with("GET /alice/repo HTTP/1.1"));
        assert!(requests[1].starts_with("GET /alice/repo/raw/e2ee/config HTTP/1.1"));
        for request in requests {
            assert!(
                request
                    .to_lowercase()
                    .contains("authorization: bearer account-token")
            );
            assert!(
                !request.contains("private-name")
                    && !request.contains("absent-name")
                    && !request.contains("http sync password")
            );
        }
        session.lock().await;
        assert!(matches!(
            client.verify().await,
            Err(EncryptedHttpError::Encryption(EncryptionError::Locked))
        ));
    }

    #[tokio::test]
    async fn descriptor_profile_identity_and_capability_mismatches_stop_before_config_or_data() {
        let mut plain = descriptor();
        plain.as_object_mut().unwrap().remove("encryption");
        let mut foreign = descriptor();
        foreign["encryption"]["uuid"] = serde_json::json!("123e4567-e89b-42d3-a456-426614174000");
        let mut unknown = descriptor();
        unknown["encryption"]["suite"] = serde_json::json!(2);
        let mut missing = descriptor();
        missing["capabilities"] = serde_json::json!([]);
        for value in [plain, foreign, unknown, missing] {
            let (url, served) = server(vec![reply(serde_json::to_vec(&value).unwrap())]).await;
            assert!(
                client(&url, session())
                    .read_ref("refs/heads/main")
                    .await
                    .is_err()
            );
            assert_eq!(served.await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn config_missing_corruption_and_oversized_responses_never_reach_data() {
        let mut corrupt = config().as_bytes().to_vec();
        corrupt[205] ^= 1;
        for response in [
            Reply {
                status: 404,
                headers: "Graft-Protocol: 1\r\n".into(),
                bytes: Vec::new(),
            },
            reply(corrupt),
            reply(vec![0; 207]),
        ] {
            let (url, served) = server(vec![
                reply(serde_json::to_vec(&descriptor()).unwrap()),
                response,
            ])
            .await;
            assert!(
                client(&url, session())
                    .read_ref("refs/heads/main")
                    .await
                    .is_err()
            );
            assert_eq!(served.await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn redirects_and_protocol_mismatches_are_rejected_without_forwarding_credentials() {
        let (url, served) = server(vec![Reply {
            status: 302,
            headers: "Graft-Protocol: 1\r\nLocation: http://127.0.0.1:1/secret\r\n".into(),
            bytes: Vec::new(),
        }])
        .await;
        assert!(matches!(
            client(&url, session()).verify().await,
            Err(EncryptedHttpError::HttpStatus(302))
        ));
        assert_eq!(served.await.unwrap().len(), 1);
        let (url, served) = server(vec![Reply {
            status: 200,
            headers: "Graft-Protocol: 1\r\nGraft-Protocol: 1\r\n".into(),
            bytes: serde_json::to_vec(&descriptor()).unwrap(),
        }])
        .await;
        assert!(matches!(
            client(&url, session()).verify().await,
            Err(EncryptedHttpError::ProtocolMismatch)
        ));
        served.await.unwrap();
    }

    #[tokio::test]
    async fn lock_cancels_an_inflight_handshake_and_waits_for_its_operation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/alice/repo", listener.local_addr().unwrap());
        let session = session();
        let client = client(&url, session.clone());
        let verifying = tokio::spawn(async move { client.verify().await });
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), session.lock())
            .await
            .unwrap();
        assert_eq!(session.state(), SecretSessionState::Locked);
        assert!(matches!(
            verifying.await.unwrap(),
            Err(EncryptedHttpError::Encryption(EncryptionError::Cancelled))
        ));
        drop(stream);
    }

    #[test]
    fn endpoint_selection_rejects_cleartext_userinfo_and_token_queries() {
        for url in [
            "http://remote.example/alice/repo",
            "https://user:secret@remote.example/alice/repo",
            "https://remote.example/alice/repo?token=secret",
            "https://remote.example/alice/repo#secret",
            "file:///tmp/remote",
        ] {
            assert!(validate_endpoint(url).is_err());
        }
        assert!(validate_endpoint("http://127.0.0.1:1/alice/repo/").is_ok());
        assert!(validate_endpoint("https://remote.example/alice/repo").is_ok());
    }
}
