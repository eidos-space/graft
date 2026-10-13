use super::{
    EncryptedConfig, EncryptionError, Result, SessionOperation, codec, session::SessionEpoch,
};
use zeroize::Zeroizing;

/// Blocking trusted native persistence. Bind precedes dispatch; only identical
/// rebinding is permitted. Retirement requires confirmed success or conflict.
pub trait ConfigMutationJournal: Send + Sync {
    fn load(&self, binding: &super::SessionBinding) -> Result<Option<ConfigMutationRecord>>;
    /// Atomically load the active candidate or last retired candidate. Retained
    /// retirement bytes support lost-ack recovery; they do not prove success.
    /// Providers without retirement receipts only recover active candidates.
    fn load_recoverable(
        &self,
        binding: &super::SessionBinding,
    ) -> Result<Option<ConfigMutationRecord>> {
        self.load(binding)
    }
    fn bind(&self, binding: &super::SessionBinding, record: &ConfigMutationRecord) -> Result<()>;
    fn retire(&self, binding: &super::SessionBinding, record: &ConfigMutationRecord) -> Result<()>;
}

pub enum ConfigPublicationOutcome {
    Confirmed,
    Unconfirmed,
    Conflict,
    OutcomeUnknown {
        error: Option<super::EncryptedHttpError>,
    },
}

/// Retained config ciphertext for a trusted, identity-bound durable journal.
/// Encoding is not authentication and contains no password or unwrapped root.
#[derive(Clone, PartialEq, Eq)]
pub struct ConfigMutationRecord {
    expected: [u8; 206],
    replacement: [u8; 206],
}

impl ConfigMutationRecord {
    pub fn expected_bytes(&self) -> &[u8] {
        &self.expected
    }
    pub fn replacement_bytes(&self) -> &[u8] {
        &self.replacement
    }

    pub fn to_local_bytes(&self) -> Result<Vec<u8>> {
        codec::encode(&[&self.expected, &self.replacement])
    }

    pub fn from_local_bytes(bytes: &[u8]) -> Result<Self> {
        let fields = codec::decode(bytes, 2, 512)?;
        let record = Self {
            expected: fields[0]
                .try_into()
                .map_err(|_| EncryptionError::InvalidEncoding)?,
            replacement: fields[1]
                .try_into()
                .map_err(|_| EncryptionError::InvalidEncoding)?,
        };
        record.configs()?;
        Ok(record)
    }

    fn configs(&self) -> Result<(EncryptedConfig, EncryptedConfig)> {
        let expected = EncryptedConfig::parse(&self.expected)?;
        let replacement = EncryptedConfig::parse(&self.replacement)?;
        if expected.repository() != replacement.repository() {
            return Err(EncryptionError::IdentityChanged);
        }
        if expected.generation().checked_add(1) != Some(replacement.generation()) {
            return Err(EncryptionError::InvalidEncoding);
        }
        Ok((expected, replacement))
    }
}

/// One exact password-change candidate, bound to its current admitted epoch.
/// Explicit journal restoration authenticates both envelopes; it never rewraps.
pub struct PreparedConfigMutation {
    epoch: SessionEpoch,
    record: ConfigMutationRecord,
}

impl PreparedConfigMutation {
    pub fn record(&self, operation: &SessionOperation) -> Result<ConfigMutationRecord> {
        if self.epoch != operation.epoch()? {
            return Err(EncryptionError::StaleObservation);
        }
        operation.check_active()?;
        Ok(self.record.clone())
    }
}

impl SessionOperation {
    /// Fresh authorization by current-password reentry. Call in an owned blocking
    /// task: KDF work must retain this operation through lock/drain and cancellation.
    /// Does not publish or advance to the candidate's checkpoint.
    pub fn prepare_password_change(
        &self,
        current: &EncryptedConfig,
        current_password: Zeroizing<Vec<u8>>,
        new_password: Zeroizing<Vec<u8>>,
    ) -> Result<PreparedConfigMutation> {
        self.check_active()?;
        super::config::validate_password(&current_password)?;
        super::config::validate_password(&new_password)?;
        current.authenticate(self.root()?)?;
        let authorized_root = current.unlock(&current_password, Some(self.root()?.repository()))?;
        drop(current_password);
        self.check_active()?;
        current.authenticate(&authorized_root)?;
        self.prepare_rewrapped_config(current, new_password)
    }

    pub(crate) fn prepare_password_reset(
        &self,
        current: &EncryptedConfig,
        new_password: Zeroizing<Vec<u8>>,
        authorizer: &dyn super::PasswordResetAuthorizer,
    ) -> Result<PreparedConfigMutation> {
        self.check_active()?;
        super::config::validate_password(&new_password)?;
        current.authenticate(self.root()?)?;
        self.observe_config(current)?;
        authorizer.authorize(self.binding())?;
        self.check_active()?;
        self.prepare_rewrapped_config(current, new_password)
    }

    fn prepare_rewrapped_config(
        &self,
        current: &EncryptedConfig,
        new_password: Zeroizing<Vec<u8>>,
    ) -> Result<PreparedConfigMutation> {
        self.observe_config(current)?;
        let candidate = current.password_change_candidate(self.root()?, &new_password)?;
        let verified_root = candidate.unlock(&new_password, Some(self.root()?.repository()))?;
        drop(new_password);
        candidate.authenticate(&verified_root)?;
        candidate.authenticate(self.root()?)?;
        self.check_active()?;
        Ok(PreparedConfigMutation {
            epoch: self.epoch()?,
            record: ConfigMutationRecord {
                expected: current
                    .as_bytes()
                    .try_into()
                    .map_err(|_| EncryptionError::InvalidEncoding)?,
                replacement: candidate
                    .as_bytes()
                    .try_into()
                    .map_err(|_| EncryptionError::InvalidEncoding)?,
            },
        })
    }

    /// Explicit trusted-journal restoration. Journal binding must be checked by
    /// the host; restored ciphertext is never a global freshness or CAS proof.
    pub fn restore_config_mutation(
        &self,
        record: ConfigMutationRecord,
    ) -> Result<PreparedConfigMutation> {
        let (expected, replacement) = record.configs()?;
        expected.authenticate(self.root()?)?;
        replacement.authenticate(self.root()?)?;
        self.check_active()?;
        Ok(PreparedConfigMutation { epoch: self.epoch()?, record })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2ee::{SecretSession, SessionBinding, tests::fixture_root};
    use std::sync::OnceLock;

    fn config() -> &'static EncryptedConfig {
        static CONFIG: OnceLock<EncryptedConfig> = OnceLock::new();
        CONFIG.get_or_init(|| {
            EncryptedConfig::create(&fixture_root(), b"mutation old password").unwrap()
        })
    }
    fn session() -> SecretSession {
        let session = SecretSession::new(
            SessionBinding::new(
                "config-mutation.test".into(),
                "account".into(),
                fixture_root().repository(),
            )
            .unwrap(),
            None,
        )
        .unwrap();
        session.unlock_with_root(config(), fixture_root()).unwrap();
        session
    }
    fn password(value: &[u8]) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(value.to_vec())
    }

    #[test]
    fn password_candidate_requires_reentry_and_restores_exact_ciphertext_in_new_epoch() {
        let session = session();
        let operation = session.begin_operation().unwrap();
        let prepared = operation
            .prepare_password_change(
                config(),
                password(b"mutation old password"),
                password(b"mutation new password"),
            )
            .unwrap();
        let record = prepared.record(&operation).unwrap();
        assert_eq!(record.expected_bytes(), config().as_bytes());
        let replacement = EncryptedConfig::parse(record.replacement_bytes()).unwrap();
        assert_eq!(replacement.generation(), config().generation() + 1);
        replacement
            .unlock(b"mutation new password", Some(fixture_root().repository()))
            .unwrap();
        assert!(replacement.unlock(b"mutation old password", None).is_err());
        let encoded = record.to_local_bytes().unwrap();
        for marker in [
            b"mutation old password".as_slice(),
            b"mutation new password".as_slice(),
        ] {
            assert!(!encoded.windows(marker.len()).any(|window| window == marker));
        }
        let raw_root: Vec<u8> = (0..32).collect();
        assert!(!encoded.windows(32).any(|window| window == raw_root));
        let peer = super::tests::session();
        let peer_operation = peer.begin_operation().unwrap();
        assert!(matches!(
            prepared.record(&peer_operation),
            Err(EncryptionError::StaleObservation)
        ));
        let restored = peer_operation
            .restore_config_mutation(ConfigMutationRecord::from_local_bytes(&encoded).unwrap())
            .unwrap();
        assert!(restored.record(&peer_operation).unwrap() == record);
        // Preparing/restoring a candidate does not advance the checkpoint to it.
        peer_operation.observe_config(config()).unwrap();
    }

    #[test]
    fn wrong_reentry_and_invalid_new_password_do_not_prepare_a_candidate() {
        let session = session();
        let operation = session.begin_operation().unwrap();
        assert!(matches!(
            operation.prepare_password_change(
                config(),
                password(b"wrong password"),
                password(b"mutation new password")
            ),
            Err(EncryptionError::UnlockFailed)
        ));
        assert!(matches!(
            operation.prepare_password_change(
                config(),
                password(b"mutation old password"),
                password(&[0xff])
            ),
            Err(EncryptionError::InvalidPassword)
        ));
        operation.observe_config(config()).unwrap();
    }

    #[test]
    fn retained_config_records_reject_forks_tampering_and_malformed_encodings() {
        let session = session();
        let operation = session.begin_operation().unwrap();
        let replacement = config()
            .password_change_candidate(&fixture_root(), b"mutation new password")
            .unwrap();
        let record = ConfigMutationRecord {
            expected: config().as_bytes().try_into().unwrap(),
            replacement: replacement.as_bytes().try_into().unwrap(),
        };
        let mut tampered = record.clone();
        tampered.replacement[174] ^= 1;
        assert!(operation.restore_config_mutation(tampered).is_err());
        let repeated = ConfigMutationRecord {
            expected: record.expected,
            replacement: record.expected,
        };
        assert!(
            ConfigMutationRecord::from_local_bytes(&repeated.to_local_bytes().unwrap()).is_err()
        );
        let bytes = record.to_local_bytes().unwrap();
        for malformed in [
            bytes[..bytes.len() - 1].to_vec(),
            [bytes.clone(), vec![0]].concat(),
            vec![0; 513],
        ] {
            assert!(ConfigMutationRecord::from_local_bytes(&malformed).is_err());
        }
        operation.observe_config(&replacement).unwrap();
        assert!(matches!(
            operation.prepare_password_change(
                config(),
                password(b"mutation old password"),
                password(b"mutation newest password")
            ),
            Err(EncryptionError::ConfigRollbackDetected)
        ));
    }
}
