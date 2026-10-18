//! Host-private durable ciphertext and exact multipart identity recovery.
use crate::e2ee::native_fs::{self as fs, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use super::{Result, SessionBinding};
use std::path::PathBuf;

/// Trusted native storage, separate from disposable staging and repository data.
/// A returned slot must be identity-bound, durable, private, and loss-detecting.
pub trait MultipartJournalProvider: Send + Sync {
    fn directory(&self, binding: &SessionBinding, opaque_key: &str) -> Result<PathBuf>;
    /// Reopen an admitted slot; absence must never allocate new recovery state.
    fn existing_directory(&self, _binding: &SessionBinding, _opaque_key: &str) -> Result<PathBuf> {
        Err(super::EncryptionError::CheckpointUnavailable(
            std::io::ErrorKind::Unsupported,
        ))
    }
}

/// Public recovery status contains opaque identity only, never logical names or secrets.
pub struct RetainedMultipartUpload {
    pub opaque_key: String,
    pub ciphertext_length: u64,
    pub server_session_recorded: bool,
}

#[cfg(any(unix, windows))]
mod file {
    use super::*;
    use crate::e2ee::{
        ChunkSize, EncryptionError, FileRefMutationJournal, FileRefMutationJournalProvider,
        ObjectReader, ObjectSnapshot, PreparedEncryptedObject, SessionOperation,
    };
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::{
        io::{Read, Seek, Write},
        path::Path,
        sync::{Arc, Mutex},
    };

    const RECORD: &str = "multipart-state";
    const MAX_RECORD: u64 = 16 * 1024;

    #[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(in crate::e2ee) struct Identity {
        pub endpoint: String,
        pub key: String,
        pub upload_id: [u8; 32],
        pub total: u64,
        pub digest: [u8; 32],
        pub logical: String,
        pub plaintext_length: u64,
        pub plaintext_digest: [u8; 32],
    }

    #[derive(Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(in crate::e2ee) struct Record {
        pub identity: Identity,
        pub session_id: Option<String>,
        pub part_bytes: Option<u64>,
        #[serde(default)]
        pub aborted_session_id: Option<String>,
        pub confirmed: bool,
    }

    pub struct FileMultipartJournalProvider {
        slots: FileRefMutationJournalProvider,
    }

    impl FileMultipartJournalProvider {
        pub fn create(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            Ok(Self {
                slots: FileRefMutationJournalProvider::create(directory, binding)?,
            })
        }
        pub fn open(directory: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            Ok(Self {
                slots: FileRefMutationJournalProvider::open(directory, binding)?,
            })
        }
        pub fn retained_uploads(
            &self,
            binding: &SessionBinding,
        ) -> Result<Vec<RetainedMultipartUpload>> {
            let mut retained = Vec::new();
            // Do not hold the namespace allocation lock while waiting for per-object locks.
            for path in self.slots.existing_private_slots(binding)? {
                let lease = FileMultipartJournal::open(path, binding)?.acquire(binding)?;
                let (identity, server_session_recorded) = match &lease.record {
                    Some(record) if record.confirmed => {
                        match fs::symlink_metadata(lease.path.join("candidate")) {
                            Ok(_) => {
                                lease.candidate_directory()?;
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(io(error)),
                        }
                        (record.identity.clone(), record.session_id.is_some())
                    }
                    Some(record) => (record.identity.clone(), record.session_id.is_some()),
                    None => match fs::symlink_metadata(lease.path.join("candidate")) {
                        Ok(_) => (lease.read_identity()?, false),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(error) => return Err(io(error)),
                    },
                };
                // An advertised retry must still have its exact identity and a
                // complete private ciphertext candidate. Full hashing remains
                // mandatory when restoring it for transport.
                if lease.read_identity()? != identity {
                    return Err(EncryptionError::InvalidEncoding);
                }
                let ciphertext =
                    fs::open(lease.candidate_directory()?.join("ciphertext")).map_err(io)?;
                if lease
                    .store
                    .validate_private_file(&ciphertext, "candidate/ciphertext")?
                    .len()
                    != identity.total
                {
                    return Err(EncryptionError::SnapshotMismatch);
                }
                crate::e2ee::object::opaque_class(&identity.key)?;
                retained.push(RetainedMultipartUpload {
                    opaque_key: identity.key,
                    ciphertext_length: identity.total,
                    server_session_recorded,
                });
            }
            retained.sort_by(|a, b| a.opaque_key.cmp(&b.opaque_key));
            Ok(retained)
        }
    }

    impl MultipartJournalProvider for FileMultipartJournalProvider {
        fn existing_directory(&self, binding: &SessionBinding, key: &str) -> Result<PathBuf> {
            crate::e2ee::object::opaque_class(key)?;
            let store = self.slots.existing_private_slot(binding, key)?;
            let (_directory, _lock, path, digest, _) = store.private_transaction(binding)?;
            read_record(&store, path, digest)?;
            Ok(path.to_owned())
        }
        fn directory(&self, binding: &SessionBinding, key: &str) -> Result<PathBuf> {
            crate::e2ee::object::opaque_class(key)?;
            let store = self.slots.private_slot(binding, key, &mut |store| {
                let (directory, _lock, path, digest, _) = store.private_transaction(binding)?;
                match fs::symlink_metadata(path.join(RECORD)) {
                    Ok(_) => {
                        read_record(store, path, digest)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        write_record(path, &directory, digest, &None, true)?;
                    }
                    Err(error) => return Err(io(error)),
                }
                Ok(())
            })?;
            let (_directory, _lock, path, digest, _) = store.private_transaction(binding)?;
            read_record(&store, path, digest)?;
            Ok(path.to_owned())
        }
    }

    /// An admitted per-object slot. Acquiring it serializes this object only.
    pub struct FileMultipartJournal {
        store: FileRefMutationJournal,
    }

    pub(in crate::e2ee) struct Lease {
        store: FileRefMutationJournal,
        directory: File,
        _lock: File,
        path: PathBuf,
        binding: [u8; 32],
        owner: u32,
        pub record: Option<Record>,
    }

    impl FileMultipartJournal {
        pub fn open(path: impl AsRef<Path>, binding: &SessionBinding) -> Result<Self> {
            let store = FileRefMutationJournal::open(path, binding)?;
            let (_directory, _lock, path, digest, _) = store.private_transaction(binding)?;
            read_record(&store, path, digest)?;
            Ok(Self { store })
        }

        pub(in crate::e2ee) fn acquire(self, binding: &SessionBinding) -> Result<Lease> {
            let (directory, lock, path, digest, owner) = self.store.private_transaction(binding)?;
            let record = read_record(&self.store, path, digest)?;
            let path = path.to_owned();
            Ok(Lease {
                store: self.store,
                directory,
                _lock: lock,
                path,
                binding: digest,
                owner,
                record,
            })
        }
    }

    impl Lease {
        fn candidate_directory(&self) -> Result<PathBuf> {
            let path = self.path.join("candidate");
            let metadata = fs::symlink_metadata(&path).map_err(io)?;
            if !metadata.is_dir() || metadata.mode() & 0o077 != 0 || metadata.uid() != self.owner {
                return Err(permission());
            }
            Ok(path)
        }

        fn read_identity(&self) -> Result<Identity> {
            let path = self.candidate_directory()?;
            let file = fs::open(path.join("identity")).map_err(io)?;
            let size = self
                .store
                .validate_private_file(&file, "candidate/identity")?
                .len();
            if size > MAX_RECORD {
                return Err(EncryptionError::TooLarge);
            }
            let mut bytes = Vec::new();
            (&file)
                .take(MAX_RECORD + 1)
                .read_to_end(&mut bytes)
                .map_err(io)?;
            if bytes.len() as u64 != size {
                return Err(EncryptionError::InvalidEncoding);
            }
            // A visible file may have survived an interrupted durability acknowledgement.
            file.sync_all().map_err(io)?;
            serde_json::from_slice(&bytes).map_err(|_| EncryptionError::InvalidEncoding)
        }

        pub fn retain(
            &mut self,
            operation: &SessionOperation,
            endpoint: &str,
            candidate: &PreparedEncryptedObject,
        ) -> Result<Option<PreparedEncryptedObject>> {
            operation.check_active()?;
            if self.record.is_none() {
                match fs::symlink_metadata(self.path.join("candidate")) {
                    Ok(_) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        self.install_candidate(operation, endpoint, candidate)?;
                    }
                    Err(error) => return Err(io(error)),
                }
                let identity = self.read_identity()?;
                self.record = Some(Record {
                    identity,
                    session_id: None,
                    part_bytes: None,
                    aborted_session_id: None,
                    confirmed: false,
                });
                self.persist()?;
            }
            let record = self
                .record
                .as_ref()
                .ok_or(EncryptionError::InvalidEncoding)?;
            let identity = &record.identity;
            if identity.endpoint != endpoint
                || identity.key != candidate.remote_key()
                || identity.logical != candidate.snapshot.logical_key
                || identity.plaintext_length != candidate.snapshot.plaintext_length
                || identity.plaintext_digest != candidate.snapshot.plaintext_sha256
            {
                return Err(EncryptionError::ImmutableConflict);
            }
            self.restore(operation, endpoint, candidate.remote_key())
        }

        pub fn restore(
            &mut self,
            operation: &SessionOperation,
            endpoint: &str,
            key: &str,
        ) -> Result<Option<PreparedEncryptedObject>> {
            operation.check_active()?;
            if self.record.is_none() {
                // The directory rename may have committed before its record acknowledgement.
                // Only an existing candidate can be adopted; restore never installs one.
                let identity = self.read_identity()?;
                if identity.endpoint != endpoint || identity.key != key {
                    return Err(EncryptionError::IdentityChanged);
                }
                self.record = Some(Record {
                    identity,
                    session_id: None,
                    part_bytes: None,
                    aborted_session_id: None,
                    confirmed: false,
                });
                self.persist()?;
            }
            let record = self
                .record
                .as_ref()
                .ok_or(EncryptionError::StaleObservation)?;
            let identity = &record.identity;
            if identity.endpoint != endpoint || identity.key != key {
                return Err(EncryptionError::IdentityChanged);
            }
            if record.confirmed {
                self.persist()?;
                return Ok(None);
            }
            if self.read_identity()? != *identity {
                return Err(EncryptionError::InvalidEncoding);
            }
            let mut file = fs::open(self.candidate_directory()?.join("ciphertext")).map_err(io)?;
            if self
                .store
                .validate_private_file(&file, "candidate/ciphertext")?
                .len()
                != identity.total
            {
                return Err(EncryptionError::SnapshotMismatch);
            }
            let mut digest = Sha256::new();
            let mut buffer = [0; 64 * 1024];
            loop {
                operation.check_active()?;
                let count = file.read(&mut buffer).map_err(io)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            if digest.finalize().as_slice() != identity.digest {
                return Err(EncryptionError::SnapshotMismatch);
            }
            file.rewind().map_err(io)?;
            let mut prefix = [0; 72];
            file.read_exact(&mut prefix).map_err(io)?;
            let header_length = ObjectReader::header_length(&prefix)?;
            let mut header = vec![0; header_length];
            header[..72].copy_from_slice(&prefix);
            file.read_exact(&mut header[72..]).map_err(io)?;
            let reader =
                ObjectReader::discover(operation.root()?, &identity.key, &header, identity.total)?;
            if prefix[12..44] != identity.upload_id
                || reader.logical_key() != identity.logical
                || reader.plaintext_length() != identity.plaintext_length
                || reader.plaintext_digest() != &identity.plaintext_digest
            {
                return Err(EncryptionError::SnapshotMismatch);
            }
            let chunk_size = match reader.chunk_size() {
                16384 => ChunkSize::KiB16,
                65536 => ChunkSize::KiB64,
                262144 => ChunkSize::KiB256,
                _ => return Err(EncryptionError::UnsupportedProfile),
            };
            file.rewind().map_err(io)?;
            file.sync_all().map_err(io)?;
            fs::sync_directory(&fs::open(self.candidate_directory()?).map_err(io)?).map_err(io)?;
            self.persist()?;
            operation.check_active()?;
            Ok(Some(PreparedEncryptedObject {
                file: Arc::new(Mutex::new(file)),
                epoch: operation.epoch()?,
                snapshot: ObjectSnapshot {
                    logical_key: identity.logical.clone(),
                    plaintext_length: identity.plaintext_length,
                    plaintext_sha256: identity.plaintext_digest,
                    chunk_size,
                },
                remote_key: identity.key.clone(),
                ciphertext_length: identity.total,
                ciphertext_sha256: identity.digest,
            }))
        }

        fn install_candidate(
            &self,
            operation: &SessionOperation,
            endpoint: &str,
            candidate: &PreparedEncryptedObject,
        ) -> Result<()> {
            let staging = tempfile::tempdir_in(&self.path).map_err(io)?;
            fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700)).map_err(io)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(staging.path().join("ciphertext"))
                .map_err(io)?;
            let mut input = candidate.file.lock().map_err(|_| permission())?;
            input.rewind().map_err(io)?;
            let mut digest = Sha256::new();
            let mut prefix = [0; 72];
            input.read_exact(&mut prefix).map_err(io)?;
            input.rewind().map_err(io)?;
            let mut length = 0_u64;
            let mut buffer = [0; 64 * 1024];
            loop {
                operation.check_active()?;
                let count = input.read(&mut buffer).map_err(io)?;
                if count == 0 {
                    break;
                }
                length = length
                    .checked_add(count as u64)
                    .ok_or(EncryptionError::TooLarge)?;
                if length > candidate.ciphertext_length() {
                    return Err(EncryptionError::SnapshotMismatch);
                }
                digest.update(&buffer[..count]);
                output.write_all(&buffer[..count]).map_err(io)?;
            }
            if length != candidate.ciphertext_length()
                || digest.finalize().as_slice() != candidate.ciphertext_sha256()
            {
                return Err(EncryptionError::SnapshotMismatch);
            }
            output.sync_all().map_err(io)?;
            let identity = Identity {
                endpoint: endpoint.to_owned(),
                key: candidate.remote_key().to_owned(),
                upload_id: prefix[12..44]
                    .try_into()
                    .map_err(|_| EncryptionError::InvalidEncoding)?,
                total: length,
                digest: *candidate.ciphertext_sha256(),
                logical: candidate.snapshot.logical_key.clone(),
                plaintext_length: candidate.snapshot.plaintext_length,
                plaintext_digest: candidate.snapshot.plaintext_sha256,
            };
            let bytes =
                serde_json::to_vec(&identity).map_err(|_| EncryptionError::InvalidEncoding)?;
            if bytes.len() as u64 > MAX_RECORD {
                return Err(EncryptionError::TooLarge);
            }
            let mut manifest = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(staging.path().join("identity"))
                .map_err(io)?;
            manifest.write_all(&bytes).map_err(io)?;
            manifest.sync_all().map_err(io)?;
            drop(manifest);
            drop(output);
            fs::sync_directory(&fs::open(staging.path()).map_err(io)?).map_err(io)?;
            operation.check_active()?;
            fs::rename(staging.path(), self.path.join("candidate")).map_err(io)?;
            fs::sync_directory(&self.directory).map_err(io)?;
            Ok(())
        }

        pub fn persist(&self) -> Result<()> {
            write_record(
                &self.path,
                &self.directory,
                self.binding,
                &self.record,
                false,
            )
        }

        pub fn confirm(&mut self) -> Result<()> {
            self.record
                .as_mut()
                .ok_or(EncryptionError::InvalidEncoding)?
                .confirmed = true;
            self.persist()?;
            // Only a durable confirmation permits deleting the local ciphertext.
            // Keep the small identity receipt and admitted slot to detect state loss.
            let path = match self.candidate_directory() {
                Ok(path) => path,
                Err(EncryptionError::CheckpointUnavailable(std::io::ErrorKind::NotFound)) => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let directory = fs::open(&path).map_err(io)?;
            for name in ["ciphertext", "identity"] {
                let file = match fs::open(path.join(name)) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(io(error)),
                };
                self.store
                    .validate_private_file(&file, &format!("candidate/{name}"))?;
                fs::remove_file(path.join(name)).map_err(io)?;
            }
            fs::sync_directory(&directory).map_err(io)?;
            fs::remove_dir(path).map_err(io)?;
            fs::sync_directory(&self.directory).map_err(io)?;
            Ok(())
        }
    }

    fn read_record(
        store: &FileRefMutationJournal,
        path: &Path,
        binding: [u8; 32],
    ) -> Result<Option<Record>> {
        let file = fs::open(path.join(RECORD)).map_err(io)?;
        let size = store.validate_private_file(&file, RECORD)?.len();
        if !(76..=MAX_RECORD).contains(&size) {
            return Err(EncryptionError::InvalidEncoding);
        }
        let mut bytes = Vec::new();
        (&file)
            .take(MAX_RECORD + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 != size
            || &bytes[..8] != b"GRAFTMU1"
            || bytes[8..40] != binding
            || bytes[bytes.len() - 32..] != Sha256::digest(&bytes[..bytes.len() - 32])[..]
        {
            return Err(EncryptionError::InvalidEncoding);
        }
        serde_json::from_slice(&bytes[40..bytes.len() - 32])
            .map_err(|_| EncryptionError::InvalidEncoding)
    }

    fn write_record(
        path: &Path,
        directory: &File,
        binding: [u8; 32],
        record: &Option<Record>,
        initial: bool,
    ) -> Result<()> {
        let mut bytes = b"GRAFTMU1".to_vec();
        bytes.extend(binding);
        bytes.extend(serde_json::to_vec(record).map_err(|_| EncryptionError::InvalidEncoding)?);
        bytes.extend(Sha256::digest(&bytes));
        if bytes.len() as u64 > MAX_RECORD {
            return Err(EncryptionError::TooLarge);
        }
        let mut staging = fs::private_named_tempfile(path).map_err(io)?;
        staging.write_all(&bytes).map_err(io)?;
        staging.as_file().sync_all().map_err(io)?;
        if initial {
            fs::persist(staging, &path.join(RECORD), false).map_err(io)?;
        } else {
            fs::persist(staging, &path.join(RECORD), true).map_err(io)?;
        }
        fs::sync_directory(directory).map_err(io)
    }

    fn io(error: std::io::Error) -> EncryptionError {
        EncryptionError::CheckpointUnavailable(error.kind())
    }
    fn permission() -> EncryptionError {
        EncryptionError::CheckpointUnavailable(std::io::ErrorKind::PermissionDenied)
    }

    #[cfg(all(test, unix))]
    mod tests {
        use super::*;
        use crate::e2ee::{EncryptedConfig, SecretSession, tests::fixture_root};
        use std::sync::OnceLock;

        fn session() -> SecretSession {
            static CONFIG: OnceLock<EncryptedConfig> = OnceLock::new();
            let config = CONFIG.get_or_init(|| {
                EncryptedConfig::create(&fixture_root(), b"multipart synthetic password").unwrap()
            });
            let session = SecretSession::new(
                SessionBinding::new(
                    "multipart.test".into(),
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
        async fn candidate(
            operation: &SessionOperation,
            bytes: &[u8],
            staging: &Path,
        ) -> PreparedEncryptedObject {
            let mut source = bytes;
            PreparedEncryptedObject::prepare(
                operation,
                ObjectSnapshot {
                    logical_key: "store/files/multipart-private".into(),
                    plaintext_length: bytes.len() as u64,
                    plaintext_sha256: Sha256::digest(bytes).into(),
                    chunk_size: ChunkSize::KiB16,
                },
                &mut source,
                staging,
                100_000,
            )
            .await
            .unwrap()
        }
        fn directory() -> tempfile::TempDir {
            let directory = tempfile::tempdir().unwrap();
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
            directory
        }

        #[tokio::test]
        async fn retained_upload_listing_rejects_a_missing_ciphertext_file() {
            let recovery = directory();
            let staging = directory();
            let session = session();
            let operation = session.begin_operation().unwrap();
            let provider =
                FileMultipartJournalProvider::create(recovery.path(), session.binding()).unwrap();
            let prepared =
                candidate(&operation, b"retained regression bytes", staging.path()).await;
            let path = provider
                .directory(session.binding(), prepared.remote_key())
                .unwrap();
            let mut lease = FileMultipartJournal::open(&path, session.binding())
                .unwrap()
                .acquire(session.binding())
                .unwrap();
            lease
                .retain(&operation, "https://test.invalid/repo", &prepared)
                .unwrap();
            drop(lease);
            assert_eq!(
                provider.retained_uploads(session.binding()).unwrap().len(),
                1
            );
            fs::remove_file(path.join("candidate/ciphertext")).unwrap();
            assert!(provider.retained_uploads(session.binding()).is_err());
        }

        #[tokio::test]
        async fn multipart_spool_reopen_retains_exact_ciphertext_and_fails_closed_on_loss() {
            let directory = directory();
            let staging = self::directory();
            let session = session();
            let operation = session.begin_operation().unwrap();
            let provider =
                FileMultipartJournalProvider::create(directory.path(), session.binding()).unwrap();
            let unknown = format!("e2ee/files/{}", "f".repeat(64));
            let allocations = fs::read(directory.path().join("ref-allocations")).unwrap();
            assert!(
                provider
                    .existing_directory(session.binding(), &unknown)
                    .is_err()
            );
            assert_eq!(
                fs::read(directory.path().join("ref-allocations")).unwrap(),
                allocations
            );
            assert!(
                provider
                    .retained_uploads(session.binding())
                    .unwrap()
                    .is_empty()
            );
            let original =
                candidate(&operation, b"synthetic multipart plaintext", staging.path()).await;
            let path = provider
                .directory(session.binding(), original.remote_key())
                .unwrap();
            let mut lease = FileMultipartJournal::open(&path, session.binding())
                .unwrap()
                .acquire(session.binding())
                .unwrap();
            let retained = lease
                .retain(&operation, "https://test.invalid/repo", &original)
                .unwrap()
                .unwrap();
            let digest = *retained.ciphertext_sha256();
            assert_eq!(digest, *original.ciphertext_sha256());
            assert!(matches!(
                FileMultipartJournal::open(&path, session.binding()),
                Err(EncryptionError::SessionBusy)
            ));
            let changed =
                candidate(&operation, b"different synthetic snapshot", staging.path()).await;
            assert!(matches!(
                lease.retain(&operation, "https://test.invalid/repo", &changed),
                Err(EncryptionError::ImmutableConflict)
            ));
            drop(lease);
            let pending = provider.retained_uploads(session.binding()).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].opaque_key, original.remote_key());
            drop(operation);
            session.lock().await;
            let reopened = self::session();
            let operation = reopened.begin_operation().unwrap();
            let provider =
                FileMultipartJournalProvider::open(directory.path(), reopened.binding()).unwrap();
            let retry =
                candidate(&operation, b"synthetic multipart plaintext", staging.path()).await;
            assert_ne!(*retry.ciphertext_sha256(), digest);
            let path = provider
                .existing_directory(reopened.binding(), retry.remote_key())
                .unwrap();
            let mut lease = FileMultipartJournal::open(&path, reopened.binding())
                .unwrap()
                .acquire(reopened.binding())
                .unwrap();
            let restored = lease
                .restore(&operation, "https://test.invalid/repo", retry.remote_key())
                .unwrap()
                .unwrap();
            assert_eq!(*restored.ciphertext_sha256(), digest);
            restored.validate(&operation).unwrap();
            assert!(matches!(
                lease.retain(&operation, "https://another.invalid/repo", &retry),
                Err(EncryptionError::ImmutableConflict)
            ));
            drop(lease);
            let ciphertext = path.join("candidate/ciphertext");
            let valid = fs::read(&ciphertext).unwrap();
            let mut corrupted = valid.clone();
            *corrupted.last_mut().unwrap() ^= 1;
            fs::write(&ciphertext, corrupted).unwrap();
            let mut lease = FileMultipartJournal::open(&path, reopened.binding())
                .unwrap()
                .acquire(reopened.binding())
                .unwrap();
            assert!(matches!(
                lease.retain(&operation, "https://test.invalid/repo", &retry),
                Err(EncryptionError::SnapshotMismatch)
            ));
            drop(lease);
            fs::write(&ciphertext, valid).unwrap();
            fs::remove_file(path.join(RECORD)).unwrap();
            assert!(
                provider
                    .directory(reopened.binding(), retry.remote_key())
                    .is_err()
            );
            assert!(!path.join(RECORD).exists());
        }

        #[tokio::test]
        async fn multipart_atomic_candidate_recovers_before_state_ack_and_rejects_links() {
            let directory = directory();
            let staging = self::directory();
            let session = session();
            let operation = session.begin_operation().unwrap();
            let provider =
                FileMultipartJournalProvider::create(directory.path(), session.binding()).unwrap();
            let original = candidate(&operation, b"atomic install plaintext", staging.path()).await;
            let path = provider
                .directory(session.binding(), original.remote_key())
                .unwrap();
            let lease = FileMultipartJournal::open(&path, session.binding())
                .unwrap()
                .acquire(session.binding())
                .unwrap();
            lease
                .install_candidate(&operation, "https://test.invalid/repo", &original)
                .unwrap();
            drop(lease);
            let retry = candidate(&operation, b"atomic install plaintext", staging.path()).await;
            let mut lease = FileMultipartJournal::open(&path, session.binding())
                .unwrap()
                .acquire(session.binding())
                .unwrap();
            let restored = lease
                .restore(&operation, "https://test.invalid/repo", retry.remote_key())
                .unwrap()
                .unwrap();
            assert_eq!(restored.ciphertext_sha256(), original.ciphertext_sha256());
            let ciphertext = path.join("candidate/ciphertext");
            let saved = path.join("candidate/saved");
            fs::rename(&ciphertext, &saved).unwrap();
            std::os::unix::fs::symlink(&saved, &ciphertext).unwrap();
            assert!(
                lease
                    .retain(&operation, "https://test.invalid/repo", &retry)
                    .is_err()
            );
            fs::remove_file(&ciphertext).unwrap();
            fs::rename(saved, ciphertext).unwrap();
            // A process may exit after the receipt commits but before local cleanup.
            // Keep that retained ciphertext discoverable for explicit recovery.
            lease.record.as_mut().unwrap().confirmed = true;
            lease.persist().unwrap();
            drop(lease);
            assert_eq!(
                provider.retained_uploads(session.binding()).unwrap().len(),
                1
            );
            let mut lease = FileMultipartJournal::open(&path, session.binding())
                .unwrap()
                .acquire(session.binding())
                .unwrap();
            lease.confirm().unwrap();
            assert!(!path.join("candidate").exists());
            drop(lease);
            assert!(
                provider
                    .retained_uploads(session.binding())
                    .unwrap()
                    .is_empty()
            );
            let mut lease = FileMultipartJournal::open(&path, session.binding())
                .unwrap()
                .acquire(session.binding())
                .unwrap();
            assert!(
                lease
                    .retain(&operation, "https://test.invalid/repo", &retry)
                    .unwrap()
                    .is_none()
            );
            // A persisted receipt never means the transport may skip winning-object readback.
            assert!(lease.record.unwrap().confirmed);
        }
    }
}

#[cfg(any(unix, windows))]
pub use file::{FileMultipartJournal, FileMultipartJournalProvider};
#[cfg(any(unix, windows))]
pub(in crate::e2ee) use file::{Identity, Lease};
