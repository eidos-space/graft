//! Private binding of migration intent to one exact prepared configuration.
use super::{EncryptedConfig, EncryptionError, Result, SessionOperation, codec};

const DOMAIN: &[u8] = b"graft/migration-enrollment/v1";
const MAX_RECORD: usize = 16 * 1024 * 1024;

/// Wrapped configuration and private target bytes, never a root or password.
/// This record does not prove remote enrollment, publication or freshness.
pub struct MigrationEnrollmentRecord {
    config: EncryptedConfig,
    target: Vec<u8>,
}

impl MigrationEnrollmentRecord {
    /// The repository caller must validate canonical target intent separately.
    pub fn new(
        operation: &SessionOperation,
        config: EncryptedConfig,
        target: Vec<u8>,
    ) -> Result<Self> {
        if config.generation() != 1 || target.is_empty() {
            return Err(EncryptionError::InvalidEncoding);
        }
        if target.len() > MAX_RECORD - DOMAIN.len() - 206 - 14 {
            return Err(EncryptionError::TooLarge);
        }
        operation.authenticate_retained_config(&config)?;
        Ok(Self { config, target })
    }
    pub fn config(&self) -> &EncryptedConfig {
        &self.config
    }
    pub fn target_intent(&self) -> &[u8] {
        &self.target
    }
    pub fn encode_private(&self) -> Result<Vec<u8>> {
        codec::encode(&[DOMAIN, self.config.as_bytes(), &self.target])
    }
    /// Authenticate retained config without advancing the session checkpoint.
    pub fn restore(operation: &SessionOperation, bytes: &[u8]) -> Result<Self> {
        let fields = codec::decode(bytes, 3, MAX_RECORD)?;
        if fields[0] != DOMAIN {
            return Err(EncryptionError::InvalidEncoding);
        }
        Self::new(
            operation,
            EncryptedConfig::parse(fields[1])?,
            fields[2].to_vec(),
        )
    }
    /// Require exact prepared config and target bytes; never silently rebind.
    pub fn require_intent(&self, config: &EncryptedConfig, target: &[u8]) -> Result<()> {
        if self.config.as_bytes() != config.as_bytes() || self.target != target {
            return Err(EncryptionError::StaleObservation);
        }
        Ok(())
    }
}

/// Initialization journal guarded by the exact retained migration enrollment.
/// The host must serialize preparation before publication and validate private
/// directory placement. This guard revalidates source intent, not a write fence.
#[cfg(unix)]
pub struct MigrationInitializationJournal {
    repository: crate::repo::Repository,
    enrollment: super::FileMigrationEnrollmentJournal,
    target: super::FileMigrationTargetJournal,
    session: super::SecretSession,
    initialization: std::sync::Arc<dyn super::InitializationJournal>,
}

#[cfg(unix)]
impl MigrationInitializationJournal {
    pub fn new(
        repository: crate::repo::Repository,
        enrollment_directory: &std::path::Path,
        target_directory: &std::path::Path,
        session: super::SecretSession,
        initialization: std::sync::Arc<dyn super::InitializationJournal>,
    ) -> Result<Self> {
        let enrollment =
            super::FileMigrationEnrollmentJournal::open(enrollment_directory, session.binding())?;
        let target = super::FileMigrationTargetJournal::open(target_directory, session.binding())?;
        let journal = Self {
            repository,
            enrollment,
            target,
            session,
            initialization,
        };
        super::InitializationJournal::load(&journal, journal.session.binding())?;
        Ok(journal)
    }
}

#[cfg(unix)]
impl super::InitializationJournal for MigrationInitializationJournal {
    fn load(&self, binding: &super::SessionBinding) -> Result<super::InitializationRecord> {
        if binding.domain() != self.session.binding().domain()
            || binding.account() != self.session.binding().account()
            || binding.repository() != self.session.binding().repository()
        {
            return Err(EncryptionError::IdentityChanged);
        }
        let operation = std::sync::Arc::new(self.session.begin_operation()?);
        let initial = self.initialization.load(binding)?;
        if initial.intent.is_some() {
            return Err(EncryptionError::StaleObservation);
        }
        let target = self.target.load(binding)?;
        let record =
            MigrationEnrollmentRecord::restore(&operation, &self.enrollment.load(binding)?)?;
        record.require_intent(&initial.config, &target)?;
        let probe = operation.clone();
        crate::repo::with_secret_cancellation(
            std::sync::Arc::new(move || probe.check_active().is_err()),
            || self.repository.restore_migration_target_plan(&target),
        )
        .map_err(|error| {
            if matches!(error, crate::repo::RepoErr::Cancelled) {
                EncryptionError::Cancelled
            } else {
                EncryptionError::StaleObservation
            }
        })?;
        operation.check_active()?;
        Ok(initial)
    }

    fn advance(
        &self,
        binding: &super::SessionBinding,
        expected: super::InitializationStage,
        next: super::InitializationStage,
    ) -> Result<()> {
        // A retained intent is checked again before recording a remote effect.
        // Changing the source leaves publication pending rather than rebinding it.
        let operation = self.session.begin_operation()?;
        self.load(binding)?;
        self.initialization.advance(binding, expected, next)?;
        operation.check_active()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2ee::{SecretSession, SessionBinding, tests::fixture_root};

    #[test]
    fn enrollment_preserves_exact_config_target_and_authenticates_before_use() {
        let root = fixture_root();
        let config = EncryptedConfig::create(&root, b"enrollment fixture password").unwrap();
        let alternate = EncryptedConfig::create(&root, b"different fixture wrapper").unwrap();
        let session = SecretSession::new(
            SessionBinding::new(
                "enrollment.test".into(),
                "account".into(),
                root.repository(),
            )
            .unwrap(),
            None,
        )
        .unwrap();
        session.unlock_with_root(&config, root).unwrap();
        let operation = session.begin_operation().unwrap();
        let target = b"canonical target fixture".to_vec();
        let record =
            MigrationEnrollmentRecord::new(&operation, config.clone(), target.clone()).unwrap();
        let encoded = record.encode_private().unwrap();
        let restored = MigrationEnrollmentRecord::restore(&operation, &encoded).unwrap();
        assert_eq!(restored.config().as_bytes(), config.as_bytes());
        restored.require_intent(&config, &target).unwrap();
        assert!(restored.require_intent(&alternate, &target).is_err());
        assert!(restored.require_intent(&config, b"changed target").is_err());
        assert!(MigrationEnrollmentRecord::new(&operation, config.clone(), Vec::new()).is_err());
        assert!(
            MigrationEnrollmentRecord::new(&operation, config.clone(), vec![0; MAX_RECORD])
                .is_err()
        );
        for length in 0..encoded.len() {
            assert!(MigrationEnrollmentRecord::restore(&operation, &encoded[..length]).is_err());
        }
        let mut bad_config = config.as_bytes().to_vec();
        bad_config[205] ^= 1;
        let malformed = codec::encode(&[DOMAIN, &bad_config, &target]).unwrap();
        assert!(matches!(
            MigrationEnrollmentRecord::restore(&operation, &malformed),
            Err(EncryptionError::AuthenticationFailed)
        ));
        let mut trailing = encoded;
        trailing.push(0);
        assert!(MigrationEnrollmentRecord::restore(&operation, &trailing).is_err());
    }
}
