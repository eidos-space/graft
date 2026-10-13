use super::{EncryptedRef, EncryptionError, RefObservation, RefTarget, SessionOperation};

/// Host-private retained ciphertext, not a session handle or a freshness proof.
/// Restoration authenticates both envelopes under a freshly admitted session.
#[derive(Clone, PartialEq, Eq)]
pub struct RefMutationRecord {
    pub(super) key: String,
    pub(super) expected: Option<Vec<u8>>,
    pub(super) replacement: Option<Vec<u8>>,
}

impl RefMutationRecord {
    /// Encode ciphertext for a trusted host journal. The provider must bind it
    /// to its account/repository slot; this encoding supplies no authenticity.
    pub fn to_local_bytes(&self) -> super::Result<Vec<u8>> {
        super::codec::encode(&[
            self.key.as_bytes(),
            &[u8::from(self.expected.is_some())],
            self.expected.as_deref().unwrap_or_default(),
            &[u8::from(self.replacement.is_some())],
            self.replacement.as_deref().unwrap_or_default(),
        ])
    }

    /// Parse bounded local metadata. Session restoration still authenticates
    /// the envelopes; parsing must never authorize publication by itself.
    pub fn from_local_bytes(bytes: &[u8]) -> super::Result<Self> {
        let fields = super::codec::decode(bytes, 5, 40 * 1024)?;
        let key = std::str::from_utf8(fields[0]).map_err(|_| EncryptionError::InvalidEncoding)?;
        super::reference::validate_remote_key(key)?;
        let envelope = |flag: &[u8], value: &[u8]| match flag {
            [0] if value.is_empty() => Ok(None),
            [1] if !value.is_empty() && value.len() <= 16 * 1024 => Ok(Some(value.to_vec())),
            _ => Err(EncryptionError::InvalidEncoding),
        };
        Ok(Self {
            key: key.to_owned(),
            expected: envelope(fields[1], fields[2])?,
            replacement: envelope(fields[3], fields[4])?,
        })
    }
}

/// One immutable candidate bound to an exact observation and unlock epoch.
/// Reusing this value retries identical ciphertext; it never regenerates a nonce.
pub struct PreparedRefMutation {
    pub(super) logical: String,
    pub(super) expected: RefObservation,
    replacement: Option<EncryptedRef>,
}

impl PreparedRefMutation {
    pub(crate) fn replacement_target(
        &self,
        operation: &SessionOperation,
    ) -> super::Result<Option<RefTarget>> {
        self.validate(operation)?;
        self.replacement_bytes()
            .map(|bytes| {
                EncryptedRef::open(operation.root()?, &self.logical, bytes)
                    .map(|(_, target)| target)
            })
            .transpose()
    }
    pub(super) fn record(&self, operation: &SessionOperation) -> super::Result<RefMutationRecord> {
        self.validate(operation)?;
        Ok(RefMutationRecord {
            key: self.remote_key().to_owned(),
            expected: self.expected_bytes().map(Vec::from),
            replacement: self.replacement_bytes().map(Vec::from),
        })
    }

    pub(super) fn restore(
        operation: &SessionOperation,
        logical: &str,
        record: RefMutationRecord,
    ) -> super::Result<Self> {
        let expected = operation.observe_ref(logical, record.expected.as_deref())?;
        if expected.remote_key() != record.key {
            return Err(EncryptionError::StaleObservation);
        }
        let replacement = record
            .replacement
            .as_deref()
            .map(|bytes| {
                EncryptedRef::open(operation.root()?, logical, bytes).map(|(sealed, _)| sealed)
            })
            .transpose()?;
        operation.check_active()?;
        Ok(Self {
            logical: logical.to_owned(),
            expected,
            replacement,
        })
    }
    pub(super) fn update(
        operation: &SessionOperation,
        logical: &str,
        target: &RefTarget,
        expected: RefObservation,
    ) -> super::Result<Self> {
        let replacement = operation.seal_ref(logical, target, &expected)?;
        Ok(Self {
            logical: logical.to_owned(),
            expected,
            replacement: Some(replacement),
        })
    }

    pub(super) fn delete(
        operation: &SessionOperation,
        logical: &str,
        expected: RefObservation,
    ) -> super::Result<Self> {
        operation.validate_observation(logical, &expected)?;
        operation.check_active()?;
        Ok(Self {
            logical: logical.to_owned(),
            expected,
            replacement: None,
        })
    }

    pub(super) fn validate(&self, operation: &SessionOperation) -> super::Result<()> {
        operation.validate_observation(&self.logical, &self.expected)?;
        operation.check_active()
    }

    pub fn remote_key(&self) -> &str {
        self.expected.remote_key()
    }
    pub fn expected_bytes(&self) -> Option<&[u8]> {
        self.expected.expected_bytes()
    }
    pub fn replacement_bytes(&self) -> Option<&[u8]> {
        self.replacement.as_ref().map(EncryptedRef::as_bytes)
    }
    pub fn is_delete(&self) -> bool {
        self.replacement.is_none()
    }
}

/// Publication state, independent of whether transport returned an error.
/// Unknown never means the candidate failed to publish; retain it for reconciliation.
pub enum RefPublicationOutcome {
    Confirmed,
    /// The authenticated current value still equals the exact old observation.
    Unconfirmed,
    /// A mismatch response plus a different authenticated current value.
    Conflict,
    OutcomeUnknown {
        error: Option<super::EncryptedHttpError>,
    },
}

pub(super) fn classify(
    observed: Option<&[u8]>,
    candidate: &PreparedRefMutation,
    mismatch: bool,
) -> RefPublicationOutcome {
    if observed == candidate.replacement_bytes() {
        return RefPublicationOutcome::Confirmed;
    }
    if observed == candidate.expected_bytes() {
        return RefPublicationOutcome::Unconfirmed;
    }
    if mismatch {
        return RefPublicationOutcome::Conflict;
    }
    RefPublicationOutcome::OutcomeUnknown { error: None }
}

impl From<EncryptionError> for RefPublicationOutcome {
    fn from(error: EncryptionError) -> Self {
        Self::OutcomeUnknown { error: Some(error.into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2ee::{EncryptedConfig, SecretSession, SessionBinding, tests::fixture_root};
    use std::sync::OnceLock;

    fn session() -> SecretSession {
        static CONFIG: OnceLock<EncryptedConfig> = OnceLock::new();
        let config = CONFIG.get_or_init(|| {
            EncryptedConfig::create(&fixture_root(), b"ref journal synthetic password").unwrap()
        });
        let session = SecretSession::new(
            SessionBinding::new(
                "ref-journal.test".into(),
                "account".into(),
                fixture_root().repository(),
            )
            .unwrap(),
            None,
        )
        .unwrap();
        session.unlock_with_root(config, fixture_root()).unwrap();
        session
    }

    #[test]
    fn retained_ref_record_authenticates_both_envelopes_before_rebinding_epoch() {
        let source = session();
        let operation = source.begin_operation().unwrap();
        let logical = "refs/heads/journal-private";
        let old =
            EncryptedRef::seal(&fixture_root(), logical, &RefTarget::Commit([7; 32])).unwrap();
        let observed = operation
            .observe_ref(logical, Some(old.as_bytes()))
            .unwrap();
        let candidate =
            PreparedRefMutation::update(&operation, logical, &RefTarget::Commit([8; 32]), observed)
                .unwrap();
        let record = candidate.record(&operation).unwrap();
        let encoded = record.to_local_bytes().unwrap();
        let resumed = session();
        let operation = resumed.begin_operation().unwrap();
        assert_eq!(
            candidate.validate(&operation),
            Err(EncryptionError::StaleObservation)
        );
        let restored = PreparedRefMutation::restore(
            &operation,
            logical,
            RefMutationRecord::from_local_bytes(&encoded).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.expected_bytes(), candidate.expected_bytes());
        assert_eq!(restored.replacement_bytes(), candidate.replacement_bytes());
        assert!(
            PreparedRefMutation::restore(&operation, "refs/heads/other", record.clone()).is_err()
        );
        for expected in [false, true] {
            let mut corrupt = record.clone();
            let bytes = if expected {
                corrupt.expected.as_mut().unwrap()
            } else {
                corrupt.replacement.as_mut().unwrap()
            };
            let end = bytes.len() - 1;
            bytes[end] ^= 1;
            assert!(matches!(
                PreparedRefMutation::restore(&operation, logical, corrupt),
                Err(EncryptionError::AuthenticationFailed)
            ));
        }
        let mut extra = encoded;
        extra.push(0);
        assert!(RefMutationRecord::from_local_bytes(&extra).is_err());
        let deletion = PreparedRefMutation::delete(
            &operation,
            logical,
            operation
                .observe_ref(logical, Some(old.as_bytes()))
                .unwrap(),
        )
        .unwrap();
        let deletion =
            PreparedRefMutation::restore(&operation, logical, deletion.record(&operation).unwrap())
                .unwrap();
        assert!(deletion.is_delete());
        assert_eq!(deletion.expected_bytes(), Some(old.as_bytes()));
    }
}
