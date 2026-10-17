//! Native cryptographic building blocks for the encrypted-remote-v1 profile.
//!
//! Native remote integration routes authenticated operations through these
//! codecs and the secret session's admission and drain boundary.

mod bundle;
mod checkpoint;
mod codec;
mod config;
mod config_mutation;
mod discovery;
mod download;
pub(crate) mod hint;
pub(crate) mod http;
mod initialization;
mod migration_enrollment;
#[cfg(unix)]
mod migration_progress;
mod multipart_journal;
mod mutation;
mod mutation_journal;
pub(crate) mod object;
mod provider;
mod reference;
mod session;
mod upload;

pub use bundle::EncryptedBundleEntry;
pub use checkpoint::CheckpointStore;
#[cfg(unix)]
pub use checkpoint::FileCheckpointStore;
pub use config::{ConfigCheckpoint, EncryptedConfig};
pub use config_mutation::{
    ConfigMutationJournal, ConfigMutationRecord, ConfigPublicationOutcome, PreparedConfigMutation,
};
pub use discovery::{DiscoveryBudget, EncryptedListClass};
pub use hint::{CommitTopology, RoutingHint};
pub use http::{
    DownloadedEncryptedObject, EncryptedHttpClient, EncryptedHttpError, EncryptedObjectMetadata,
};
pub(crate) use http::{PreparedReceiveBundle, ReceiveBundleOutcome};
#[cfg(unix)]
pub use initialization::FileInitializationJournal;
pub use initialization::{
    InitialConfigPublicationOutcome, InitializationIntent, InitializationJournal,
    InitializationRecord, InitializationStage,
};
pub use migration_enrollment::MigrationEnrollmentRecord;
#[cfg(unix)]
pub use migration_enrollment::MigrationInitializationJournal;
#[cfg(unix)]
pub use migration_progress::{
    FileMigrationProgressJournal, MigrationProgressLease, MigrationProgressRecord,
};
#[cfg(unix)]
pub use multipart_journal::{FileMultipartJournal, FileMultipartJournalProvider};
pub use multipart_journal::{MultipartJournalProvider, RetainedMultipartUpload};
pub use mutation::{PreparedRefMutation, RefMutationRecord, RefPublicationOutcome};
#[cfg(unix)]
pub use mutation_journal::{
    FileConfigMutationJournal, FileMigrationEnrollmentJournal, FileMigrationSourceJournal,
    FileMigrationTargetJournal, FileRefMutationJournal, FileRefMutationJournalProvider,
};
pub use mutation_journal::{RefMutationJournal, RefMutationJournalProvider};
pub use object::{ChunkSize, ObjectEncryptor, ObjectReader};
#[cfg(all(target_os = "macos", feature = "macos-keychain"))]
pub use provider::MacKeychainProvider;
pub use provider::{PasswordResetAuthorizer, RememberedKeyProvider};
pub use reference::{EncryptedRef, RefTarget};
pub use session::{
    RefObservation, SecretSession, SecretSessionState, SessionBinding, SessionOperation,
};
pub use upload::{ObjectPublicationOutcome, ObjectSnapshot, PreparedEncryptedObject};

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fmt;
use zeroize::Zeroizing;

pub(crate) const PROFILE: &[u8] = b"encrypted-remote-v1";
pub(crate) const SUITE: u16 = 1;
type HmacSha256 = Hmac<Sha256>;

/// Errors intentionally contain no passwords, keys or decrypted content.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EncryptionError {
    #[error("approved secure key storage unavailable")]
    SecureStorageUnavailable,
    #[error("no remembered key exists for this session binding")]
    RememberedKeyMissing,
    #[error("local encryption checkpoint storage unavailable ({0:?})")]
    CheckpointUnavailable(std::io::ErrorKind),
    #[error("encrypted synchronization is locked")]
    Locked,
    #[error("encrypted synchronization is paused")]
    Paused,
    #[error("encrypted synchronization is disconnected")]
    Disconnected,
    #[error("encryption session is busy")]
    SessionBusy,
    #[error("invalid encryption session binding")]
    InvalidSessionBinding,
    #[error("encrypted operation was cancelled")]
    Cancelled,
    #[error("encrypted observation belongs to another session or reference")]
    StaleObservation,
    #[error("invalid encrypted encoding")]
    InvalidEncoding,
    #[error("unsupported encrypted format, suite or KDF profile")]
    UnsupportedProfile,
    #[error("encrypted input exceeds its size limit")]
    TooLarge,
    #[error("password must contain 1–1024 UTF-8 bytes")]
    InvalidPassword,
    #[error("unable to unlock encrypted repository")]
    UnlockFailed,
    #[error("encrypted data authentication failed")]
    AuthenticationFailed,
    #[error("authenticated routing hint does not match canonical pack topology")]
    HintMismatch,
    #[error("remote repository identity changed")]
    IdentityChanged,
    #[error("repository configuration rolled back or forked")]
    ConfigRollbackDetected,
    #[error("configuration generation exhausted")]
    GenerationExhausted,
    #[error("operating system random source unavailable")]
    RandomUnavailable,
    #[error("invalid logical object key")]
    InvalidObjectKey,
    #[error("invalid encrypted chunk order or length")]
    InvalidChunk,
    #[error("plaintext does not match the prepared immutable snapshot")]
    SnapshotMismatch,
    #[error("existing encrypted object conflicts with the immutable snapshot")]
    ImmutableConflict,
}

pub type Result<T> = std::result::Result<T, EncryptionError>;

/// The raw UUID pinned by a native client. Not a secret or an authorization token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepositoryId([u8; 16]);

impl RepositoryId {
    pub fn generate() -> Result<Self> {
        let mut bytes = random::<16>()?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Result<Self> {
        if bytes[6] >> 4 != 4 || bytes[8] >> 6 != 2 {
            return Err(EncryptionError::InvalidEncoding);
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// Nonserializable, non-cloneable root key bound to a repository identity.
/// Providers may import controlled buffers; no plaintext export API is exposed.
pub struct RootKey {
    repository: RepositoryId,
    secret: Zeroizing<[u8; 32]>,
}

impl fmt::Debug for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootKey")
            .field("repository", &self.repository)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl RootKey {
    pub fn generate(repository: RepositoryId) -> Result<Self> {
        let mut secret = Zeroizing::new([0; 32]);
        getrandom::fill(secret.as_mut()).map_err(|_| EncryptionError::RandomUnavailable)?;
        Ok(Self { repository, secret })
    }

    pub fn import(repository: RepositoryId, secret: Zeroizing<[u8; 32]>) -> Self {
        Self { repository, secret }
    }

    pub fn repository(&self) -> RepositoryId {
        self.repository
    }

    pub(crate) fn derive(&self, purpose: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        let info = codec::encode(&[b"graft/e2ee/v1", PROFILE, &SUITE.to_be_bytes(), purpose])?;
        let mut output = Zeroizing::new([0; 32]);
        Hkdf::<Sha256>::new(Some(self.repository.as_bytes()), self.secret.as_ref())
            .expand(&info, output.as_mut())
            .map_err(|_| EncryptionError::InvalidEncoding)?;
        Ok(output)
    }

    /// Opaque synchronization token; the authoritative commit ID stays private.
    pub fn route_commit(&self, commit: &[u8; 32]) -> Result<[u8; 32]> {
        let key = self.derive(b"route")?;
        let input = codec::encode(&[b"graft/route-commit/v1", PROFILE, commit])?;
        Ok(mac(key.as_ref(), &input))
    }
}

pub(crate) fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| EncryptionError::RandomUnavailable)?;
    Ok(bytes)
}

pub(crate) fn mac(key: &[u8], input: &[u8]) -> [u8; 32] {
    let mut authenticator =
        <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    authenticator.update(input);
    authenticator.finalize().into_bytes().into()
}

pub(crate) fn verify_mac(key: &[u8], input: &[u8], expected: &[u8]) -> Result<()> {
    let mut authenticator =
        <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    authenticator.update(input);
    authenticator
        .verify_slice(expected)
        .map_err(|_| EncryptionError::AuthenticationFailed)
}

#[cfg(test)]
mod tests;
