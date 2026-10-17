//! Stable host-facing E2EE error classification, independent of message text.
#[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
use crate::SdkErrorCode;

/// Host-facing encrypted causes, available even when encrypted support is disabled.
///
/// Account authorization and encrypted-data authentication are distinct: an HTTP
/// 401/403 is `AuthenticationFailed`, while a failed cryptographic tag is
/// `IntegrityFailed`. Dispatch on this type or `as_str()`, not diagnostic text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncryptedErrorCode {
    Locked,
    Disconnected,
    SessionBusy,
    Paused,
    InvalidSessionBinding,
    InvalidPassword,
    UnlockFailed,
    RememberedKeyMissing,
    SecureStorageUnavailable,
    IdentityChanged,
    AuthenticationFailed,
    IntegrityFailed,
    HintMismatch,
    ConfigRollbackDetected,
    CheckpointUnavailable,
    StaleObservation,
    InvalidEncoding,
    UnsupportedEncryptedProfile,
    TooLarge,
    GenerationExhausted,
    RandomUnavailable,
    InvalidObjectKey,
    InvalidChunk,
    SnapshotMismatch,
    ImmutableConflict,
    RemoteConfigMissing,
    ProtocolMismatch,
    HttpStatus,
    LocalIo,
    TransportFailed,
}

impl EncryptedErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Locked => "GRAFT_SDK_E2EE_LOCKED",
            Self::Disconnected => "GRAFT_SDK_E2EE_DISCONNECTED",
            Self::SessionBusy => "GRAFT_SDK_E2EE_SESSION_BUSY",
            Self::Paused => "GRAFT_SDK_E2EE_PAUSED",
            Self::InvalidSessionBinding => "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING",
            Self::InvalidPassword => "GRAFT_SDK_E2EE_INVALID_PASSWORD",
            Self::UnlockFailed => "GRAFT_SDK_E2EE_UNLOCK_FAILED",
            Self::RememberedKeyMissing => "GRAFT_SDK_E2EE_REMEMBERED_KEY_MISSING",
            Self::SecureStorageUnavailable => "GRAFT_SDK_E2EE_SECURE_STORAGE_UNAVAILABLE",
            Self::IdentityChanged => "GRAFT_SDK_E2EE_IDENTITY_CHANGED",
            Self::AuthenticationFailed => "GRAFT_SDK_E2EE_AUTHENTICATION_FAILED",
            Self::IntegrityFailed => "GRAFT_SDK_E2EE_INTEGRITY_FAILED",
            Self::HintMismatch => "GRAFT_SDK_E2EE_HINT_MISMATCH",
            Self::ConfigRollbackDetected => "GRAFT_SDK_E2EE_CONFIG_ROLLBACK_DETECTED",
            Self::CheckpointUnavailable => "GRAFT_SDK_E2EE_CHECKPOINT_UNAVAILABLE",
            Self::StaleObservation => "GRAFT_SDK_E2EE_STALE_OBSERVATION",
            Self::InvalidEncoding => "GRAFT_SDK_E2EE_INVALID_ENCODING",
            Self::UnsupportedEncryptedProfile => "GRAFT_SDK_E2EE_UNSUPPORTED_ENCRYPTED_PROFILE",
            Self::TooLarge => "GRAFT_SDK_E2EE_TOO_LARGE",
            Self::GenerationExhausted => "GRAFT_SDK_E2EE_GENERATION_EXHAUSTED",
            Self::RandomUnavailable => "GRAFT_SDK_E2EE_RANDOM_UNAVAILABLE",
            Self::InvalidObjectKey => "GRAFT_SDK_E2EE_INVALID_OBJECT_KEY",
            Self::InvalidChunk => "GRAFT_SDK_E2EE_INVALID_CHUNK",
            Self::SnapshotMismatch => "GRAFT_SDK_E2EE_SNAPSHOT_MISMATCH",
            Self::ImmutableConflict => "GRAFT_SDK_E2EE_IMMUTABLE_CONFLICT",
            Self::RemoteConfigMissing => "GRAFT_SDK_E2EE_REMOTE_CONFIG_MISSING",
            Self::ProtocolMismatch => "GRAFT_SDK_E2EE_PROTOCOL_MISMATCH",
            Self::HttpStatus => "GRAFT_SDK_E2EE_HTTP_STATUS",
            Self::LocalIo => "GRAFT_SDK_E2EE_LOCAL_IO",
            Self::TransportFailed => "GRAFT_SDK_E2EE_TRANSPORT_FAILED",
        }
    }
}

#[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
/// Classifies a secret-session or cryptographic cause without inspecting its text.
pub fn encryption_error_code(error: &graft::e2ee::EncryptionError) -> SdkErrorCode {
    use graft::e2ee::EncryptionError as E;
    SdkErrorCode::Encrypted(match error {
        E::Locked => EncryptedErrorCode::Locked,
        E::Disconnected => EncryptedErrorCode::Disconnected,
        E::SessionBusy => EncryptedErrorCode::SessionBusy,
        E::Paused => EncryptedErrorCode::Paused,
        E::InvalidSessionBinding => EncryptedErrorCode::InvalidSessionBinding,
        E::InvalidPassword => EncryptedErrorCode::InvalidPassword,
        E::UnlockFailed => EncryptedErrorCode::UnlockFailed,
        E::RememberedKeyMissing => EncryptedErrorCode::RememberedKeyMissing,
        E::SecureStorageUnavailable => EncryptedErrorCode::SecureStorageUnavailable,
        E::IdentityChanged => EncryptedErrorCode::IdentityChanged,
        E::AuthenticationFailed => EncryptedErrorCode::IntegrityFailed,
        E::HintMismatch => EncryptedErrorCode::HintMismatch,
        E::ConfigRollbackDetected => EncryptedErrorCode::ConfigRollbackDetected,
        E::CheckpointUnavailable(_) => EncryptedErrorCode::CheckpointUnavailable,
        E::StaleObservation => EncryptedErrorCode::StaleObservation,
        E::InvalidEncoding => EncryptedErrorCode::InvalidEncoding,
        E::UnsupportedProfile => EncryptedErrorCode::UnsupportedEncryptedProfile,
        E::TooLarge => EncryptedErrorCode::TooLarge,
        E::GenerationExhausted => EncryptedErrorCode::GenerationExhausted,
        E::RandomUnavailable => EncryptedErrorCode::RandomUnavailable,
        E::InvalidObjectKey => EncryptedErrorCode::InvalidObjectKey,
        E::InvalidChunk => EncryptedErrorCode::InvalidChunk,
        E::SnapshotMismatch => EncryptedErrorCode::SnapshotMismatch,
        E::ImmutableConflict => EncryptedErrorCode::ImmutableConflict,
        E::Cancelled => return SdkErrorCode::Cancelled,
    })
}

#[cfg(all(feature = "e2ee", not(target_arch = "wasm32")))]
/// Classifies an encrypted HTTP cause; publication wrappers must be handled first.
pub fn encrypted_http_error_code(error: &graft::e2ee::EncryptedHttpError) -> SdkErrorCode {
    use graft::e2ee::EncryptedHttpError as E;
    SdkErrorCode::Encrypted(match error {
        E::Encryption(error) => return encryption_error_code(error),
        E::HttpStatus(401 | 403) => EncryptedErrorCode::AuthenticationFailed,
        E::HttpStatus(_) => EncryptedErrorCode::HttpStatus,
        E::ConfigMissing => EncryptedErrorCode::RemoteConfigMissing,
        E::UnsupportedProfile => EncryptedErrorCode::UnsupportedEncryptedProfile,
        E::InvalidEndpoint => return SdkErrorCode::InvalidArgument,
        E::ProtocolMismatch => EncryptedErrorCode::ProtocolMismatch,
        E::LocalIo(_) => EncryptedErrorCode::LocalIo,
        E::Transport(error) if error.is_timeout() => return SdkErrorCode::RemoteTransportTimeout,
        E::Transport(_) => EncryptedErrorCode::TransportFailed,
    })
}
